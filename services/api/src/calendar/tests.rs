use axum::extract::Path as RoutePath;
use rusttalk_core::{
    calendar::{Answer, Details},
    identity::DeviceIdentity,
    keys::{Key, random_key},
};

use super::*;
use crate::testing::{self, as_person};

const OWNER: u8 = 2;
const MOD: u8 = 3;
const MEMBER: u8 = 4;
const OTHER: u8 = 5;
const NO_ROLE: u8 = 6;

struct Server {
    app: App,
    key: Key,
    people: Vec<DeviceIdentity>,
    _dir: tempfile::TempDir,
}

fn code<T>(result: ApiResult<T>) -> &'static str {
    result.err().expect("an error").code
}

impl Server {
    /// Signed events need real certificates, so these people have real identities.
    async fn new() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let app = testing::app(dir.path(), 1).await?;
        let space = keys::space(&app).id;
        let mut tx = store::write(&app.db).await?;
        keys::init_space(&mut tx, &space)
            .await
            .expect("space scope");
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(encode(&space))
            .execute(&mut *tx)
            .await?;
        let mut people = Vec::new();
        for who in OWNER..=NO_ROLE {
            let (_, identity) = DeviceIdentity::create("test")?;
            let cert = &identity.certificate;
            let person = cert.person_id().to_vec();
            let device = cert.signing_key().to_bytes().to_vec();
            query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
                .bind(&person)
                .execute(&mut *tx)
                .await?;
            query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
                .bind(store::hash(&[who; 32]).as_slice())
                .bind(&person)
                .bind(&device)
                .bind(i64::MAX)
                .bind(vec![who])
                .execute(&mut *tx)
                .await?;
            query(
                "INSERT INTO member_devices (device_id, person_id, certificate) VALUES (?, ?, ?)",
            )
            .bind(&device)
            .bind(&person)
            .bind(cert.public_bytes()?)
            .execute(&mut *tx)
            .await?;
            let role = match who {
                OWNER => {
                    query("UPDATE server_settings SET owner_person_id = ?")
                        .bind(&person)
                        .execute(&mut *tx)
                        .await?;
                    None
                }
                MOD => Some("moderator"),
                MEMBER | OTHER => Some("member"),
                _ => None,
            };
            if let Some(role) = role {
                query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
                    .bind(&person)
                    .bind(role)
                    .execute(&mut *tx)
                    .await?;
            }
            people.push(identity);
        }
        tx.commit().await?;
        Ok(Self {
            app,
            key: random_key()?,
            people,
            _dir: dir,
        })
    }

    async fn switch(&self, who: u8, enabled: bool) -> ApiResult<StatusCode> {
        let body = Ok(Json(CalendarSetting { enabled }));
        set_enabled(State(self.app.clone()), as_person(who), body).await
    }

    fn person(&self, who: u8) -> &DeviceIdentity {
        &self.people[usize::from(who - OWNER)]
    }

    fn event(&self, who: u8, id: u8, ends_in: i64) -> NewEvent {
        let author = self.person(who);
        let end = store::now().expect("clock") + ends_in;
        let details = Details {
            title: "Quiz".into(),
            description: "Bring snacks".into(),
            start: end - 3600,
            end,
            ..Details::default()
        };
        let context = EventContext {
            server_id: keys::space(&self.app).id,
            id: [id; 32],
            epoch: 1,
            remove_at: end + GRACE,
            author_device: author.certificate.signing_key().to_bytes(),
            author: *author.certificate.person_id(),
        };
        NewEvent {
            id: context.id,
            epoch: 1,
            remove_at: context.remove_at,
            envelope: encode(&context.seal(&self.key, author, &details).expect("sealed")),
        }
    }

    async fn post(&self, who: u8, new: NewEvent) -> ApiResult<EventRecord> {
        create(State(self.app.clone()), as_person(who), Ok(Json(new)))
            .await
            .map(|(_, Json(record))| record)
    }

    async fn add(&self, who: u8, id: u8) -> ApiResult<EventRecord> {
        self.post(who, self.event(who, id, 86_400)).await
    }

    async fn approve(&self, who: u8, id: u8) -> ApiResult<StatusCode> {
        let path = RoutePath(encode(&[id; 32]));
        approve(State(self.app.clone()), as_person(who), path).await
    }

    async fn remove(&self, who: u8, id: u8) -> ApiResult<StatusCode> {
        let path = RoutePath(encode(&[id; 32]));
        remove(State(self.app.clone()), as_person(who), path).await
    }

    async fn seen(&self, who: u8) -> Vec<u8> {
        let Json(list) = list(State(self.app.clone()), as_person(who))
            .await
            .expect("calendar");
        let mut ids: Vec<u8> = list.events.iter().map(|event| event.id[0]).collect();
        ids.sort_unstable();
        ids
    }
}

#[tokio::test]
async fn off_until_someone_who_manages_the_server_turns_it_on() -> anyhow::Result<()> {
    let server = Server::new().await?;
    assert_eq!(code(server.add(OWNER, 10).await), "events_disabled");
    for who in [MOD, MEMBER] {
        assert_eq!(code(server.switch(who, true).await), "permission_denied");
    }
    server.switch(OWNER, true).await?;
    server.add(MEMBER, 10).await?;
    assert_eq!(server.seen(MEMBER).await, [10]);

    server.switch(OWNER, false).await?;
    assert!(server.seen(OWNER).await.is_empty());
    assert_eq!(code(server.approve(MOD, 10).await), "events_disabled");
    server.remove(MEMBER, 10).await?;
    Ok(())
}

#[tokio::test]
async fn suggestions_need_approval_and_only_managers_give_it() -> anyhow::Result<()> {
    let server = Server::new().await?;
    server.switch(OWNER, true).await?;
    assert!(!server.add(MEMBER, 10).await?.approved);
    assert!(server.add(MOD, 11).await?.approved);
    assert!(server.add(OWNER, 12).await?.approved);
    assert_eq!(code(server.add(NO_ROLE, 13).await), "permission_denied");

    assert_eq!(server.seen(MEMBER).await, [10, 11, 12]);
    assert_eq!(server.seen(MOD).await, [10, 11, 12]);
    assert_eq!(server.seen(OTHER).await, [11, 12]);

    assert_eq!(code(server.approve(MEMBER, 10).await), "permission_denied");
    assert_eq!(code(server.remove(OTHER, 10).await), "event_not_found");
    assert_eq!(code(server.remove(OTHER, 11).await), "permission_denied");
    server.approve(MOD, 10).await?;
    assert_eq!(code(server.approve(MOD, 10).await), "event_approved");
    assert_eq!(server.seen(OTHER).await, [10, 11, 12]);

    server.remove(MEMBER, 10).await?;
    server.remove(MOD, 12).await?;
    assert_eq!(server.seen(OTHER).await, [11]);
    assert_eq!(code(server.add(MEMBER, 10).await), "event_exists");
    Ok(())
}

#[tokio::test]
async fn create_and_manage_are_separate_permissions() -> anyhow::Result<()> {
    let server = Server::new().await?;
    server.switch(OWNER, true).await?;
    server.add(MEMBER, 10).await?;
    for (who, role, bits) in [(OTHER, "creator", 8192), (NO_ROLE, "manager", 16384)] {
        query("INSERT INTO roles (id, kind, rank, permissions) VALUES (?, 'custom', 5, ?)")
            .bind(role)
            .bind(bits)
            .execute(&server.app.db)
            .await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
            .bind(server.person(who).certificate.person_id().as_slice())
            .bind(role)
            .execute(&server.app.db)
            .await?;
    }
    assert!(server.add(OTHER, 11).await?.approved);
    assert_eq!(code(server.approve(OTHER, 10).await), "permission_denied");
    assert_eq!(code(server.add(NO_ROLE, 12).await), "permission_denied");
    server.approve(NO_ROLE, 10).await?;
    Ok(())
}

#[tokio::test]
async fn refuses_bad_times_epochs_and_signatures() -> anyhow::Result<()> {
    let server = Server::new().await?;
    server.switch(OWNER, true).await?;
    for ends_in in [-60, HORIZON + 3600] {
        let new = server.event(MEMBER, 10, ends_in);
        assert_eq!(code(server.post(MEMBER, new).await), "invalid_event_time");
    }
    let mut stale = server.event(MEMBER, 10, 3600);
    stale.epoch = 2;
    assert_eq!(code(server.post(MEMBER, stale).await), "epoch_changed");
    let mut later = server.event(MEMBER, 10, 3600);
    later.remove_at += 60;
    assert_eq!(code(server.post(MEMBER, later).await), "invalid_event");
    let someone_elses = server.event(OTHER, 10, 3600);
    assert_eq!(
        code(server.post(MEMBER, someone_elses).await),
        "invalid_event"
    );
    Ok(())
}

#[tokio::test]
async fn limits_waiting_suggestions_and_all_events() -> anyhow::Result<()> {
    let server = Server::new().await?;
    server.switch(OWNER, true).await?;
    let waiting = MAX_WAITING as u8;
    for id in 10..10 + waiting {
        server.add(MEMBER, id).await?;
    }
    assert_eq!(code(server.add(MEMBER, 100).await), "suggestion_limit");
    for id in 10 + waiting..10 + MAX_EVENTS as u8 {
        server.add(MOD, id).await?;
    }
    assert_eq!(code(server.add(OWNER, 100).await), "event_limit");
    Ok(())
}

#[tokio::test]
async fn events_go_two_hours_after_their_end() -> anyhow::Result<()> {
    let server = Server::new().await?;
    server.switch(OWNER, true).await?;
    for id in [10, 11, 12] {
        server.add(MEMBER, id).await?;
    }
    let now = store::now().expect("clock");
    for (id, remove_at) in [(10, now + GRACE - 60), (11, now - 1)] {
        query("UPDATE calendar_events SET remove_at = ? WHERE id = ?")
            .bind(remove_at)
            .bind(encode(&[id; 32]))
            .execute(&server.app.db)
            .await?;
    }
    assert_eq!(code(server.approve(MOD, 10).await), "event_ended");
    // Due ones are hidden even before the next sweep deletes them.
    assert_eq!(server.seen(MOD).await, [10, 12]);
    expire(&server.app).await?;
    let left: i64 = query_scalar("SELECT COUNT(*) FROM calendar_events")
        .fetch_one(&server.app.db)
        .await?;
    assert_eq!(left, 2);
    Ok(())
}

mod upgrade;

impl Server {
    fn sealed_answer(&self, who: u8, id: u8, answer: Answer) -> NewAnswer {
        let person = self.person(who);
        let context = AnswerContext {
            server_id: keys::space(&self.app).id,
            event_id: [id; 32],
            epoch: 1,
            device: person.certificate.signing_key().to_bytes(),
            person: *person.certificate.person_id(),
        };
        NewAnswer {
            epoch: 1,
            envelope: encode(&context.seal(&self.key, person, answer).expect("sealed")),
        }
    }

    async fn answer(&self, who: u8, id: u8, new: NewAnswer) -> ApiResult<StatusCode> {
        let path = RoutePath(encode(&[id; 32]));
        answer(State(self.app.clone()), as_person(who), path, Ok(Json(new))).await
    }

    async fn remind(&self, who: u8, id: u8) -> ApiResult<StatusCode> {
        let path = RoutePath(encode(&[id; 32]));
        remind(State(self.app.clone()), as_person(who), path).await
    }

    async fn record(&self, who: u8, id: u8) -> EventRecord {
        let Json(list) = list(State(self.app.clone()), as_person(who))
            .await
            .expect("calendar");
        let found = list.events.into_iter().find(|event| event.id == [id; 32]);
        found.expect("event listed")
    }
}

#[tokio::test]
async fn people_answer_approved_events_and_change_their_mind() -> anyhow::Result<()> {
    let server = Server::new().await?;
    server.switch(OWNER, true).await?;
    server.add(MOD, 10).await?;
    server.add(MEMBER, 11).await?;

    let going = server.sealed_answer(OTHER, 10, Answer::Going);
    server.answer(OTHER, 10, going).await?;
    let not = server.sealed_answer(MEMBER, 10, Answer::NotGoing);
    server.answer(MEMBER, 10, not).await?;
    let changed = server.sealed_answer(OTHER, 10, Answer::NotGoing);
    server.answer(OTHER, 10, changed).await?;
    let record = server.record(MOD, 10).await;
    assert_eq!(
        record.answers.len(),
        2,
        "a second answer replaces the first"
    );
    let other = server.person(OTHER).certificate.person_id();
    let stored = record.answers.iter().find(|a| &a.person == other).unwrap();
    let context = AnswerContext {
        server_id: keys::space(&server.app).id,
        event_id: [10; 32],
        epoch: 1,
        device: stored.device,
        person: stored.person,
    };
    let envelope = rusttalk_core::auth::decode_bytes(&stored.envelope)?;
    let opened = context.open(&server.key, &server.person(OTHER).certificate, &envelope)?;
    assert_eq!(opened, Answer::NotGoing);

    // A suggestion waits for approval before anyone answers it.
    let early = server.sealed_answer(OTHER, 11, Answer::Going);
    assert_eq!(
        code(server.answer(OTHER, 11, early).await),
        "event_not_found"
    );
    let forged = server.sealed_answer(MEMBER, 10, Answer::Going);
    assert_eq!(
        code(server.answer(OTHER, 10, forged).await),
        "invalid_answer"
    );

    // Answers go with their event.
    server.remove(MOD, 10).await?;
    let left: i64 = query_scalar("SELECT COUNT(*) FROM calendar_answers")
        .fetch_one(&server.app.db)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}

#[tokio::test]
async fn the_author_or_a_manager_reminds_at_most_once_an_hour() -> anyhow::Result<()> {
    let server = Server::new().await?;
    server.switch(OWNER, true).await?;
    server.add(MOD, 10).await?;
    server.add(MEMBER, 11).await?;
    server.approve(MOD, 11).await?;

    assert_eq!(code(server.remind(OTHER, 10).await), "permission_denied");
    let (author, manager) = tokio::join!(server.remind(MEMBER, 11), server.remind(MOD, 11));
    match (author, manager) {
        (Ok(_), rejected) | (rejected, Ok(_)) => assert_eq!(code(rejected), "remind_too_soon"),
        (Err(_), Err(_)) => panic!("one concurrent reminder should succeed"),
    }
    assert!(server.record(OTHER, 11).await.reminded_at.is_some());
    assert_eq!(code(server.remind(MOD, 11).await), "remind_too_soon");
    query("UPDATE calendar_events SET reminded_at = reminded_at - ? WHERE id = ?")
        .bind(REMIND_EVERY)
        .bind(encode(&[11; 32]))
        .execute(&server.app.db)
        .await?;
    server.remind(MOD, 11).await?;

    query("UPDATE calendar_events SET remove_at = ? WHERE id = ?")
        .bind(store::now().expect("clock") + GRACE - 60)
        .bind(encode(&[10; 32]))
        .execute(&server.app.db)
        .await?;
    assert_eq!(code(server.remind(MOD, 10).await), "event_ended");
    Ok(())
}
