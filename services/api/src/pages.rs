//! Page documents: the server stores the newest ciphertext of each page channel.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use ed25519_dalek::VerifyingKey;
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    keys::{Event, Kind, Scope},
    pages::{PageContext, PageRecord, SavePage, valid_step},
};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    keys, messages, page_files, payload, store,
};

#[derive(FromRow)]
struct Row {
    version: i64,
    epoch: i64,
    sender_person: Vec<u8>,
    sender_device: Vec<u8>,
    at: i64,
    envelope: Vec<u8>,
}

impl TryFrom<Row> for PageRecord {
    type Error = ApiError;

    fn try_from(row: Row) -> ApiResult<Self> {
        let invalid = |_| ApiError::internal("invalid_stored_sender");
        Ok(Self {
            version: row.version,
            epoch: row.epoch,
            person: row.sender_person.try_into().map_err(invalid)?,
            sender_device: row.sender_device.try_into().map_err(invalid)?,
            at: row.at,
            envelope: encode(&row.envelope),
        })
    }
}

pub async fn is_page(db: &mut SqliteConnection, channel: &str) -> ApiResult<bool> {
    Ok(
        query_scalar("SELECT EXISTS (SELECT 1 FROM channels WHERE id = ? AND page)")
            .bind(channel)
            .fetch_one(db)
            .await?,
    )
}

async fn require_page(db: &mut SqliteConnection, channel: &str) -> ApiResult<()> {
    if is_page(db, channel).await? {
        Ok(())
    } else {
        Err(ApiError::not_found("page_not_found"))
    }
}

pub async fn get(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Response> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    messages::chat_kind(&mut tx, &actor, &channel).await?;
    require_page(&mut tx, &channel).await?;
    let row: Option<Row> = query_as(
        "SELECT version, epoch, sender_person, sender_device, at, envelope
         FROM page_documents WHERE channel_id = ?",
    )
    .bind(&channel)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(match row {
        Some(row) => Json(PageRecord::try_from(row)?).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

pub async fn put(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    body: Result<Json<SavePage>, JsonRejection>,
) -> ApiResult<Json<PageRecord>> {
    let SavePage {
        base,
        version,
        epoch,
        envelope,
        keep,
    } = payload(body)?;
    let channel_id = decode(&channel)?;
    let bytes = decode_bytes(&envelope)?;
    if !valid_step(base, version) {
        return Err(ApiError::bad_request("invalid_page_version"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    messages::require_write(&mut tx, &actor, &channel).await?;
    require_page(&mut tx, &channel).await?;
    let scope = Scope {
        kind: Kind::Channel,
        id: channel_id,
    };
    let (current, rotate) = keys::read_epoch(&mut tx, &actor, scope).await?;
    if rotate || epoch != current {
        return Err(keys::conflict("epoch_changed"));
    }
    let device = actor
        .device
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::internal("invalid_stored_device"))?;
    let verification_key = VerifyingKey::from_bytes(&device)
        .map_err(|_| ApiError::internal("invalid_stored_device"))?;
    let context = PageContext {
        server_id: keys::space(&app).id,
        channel_id,
        epoch,
        version,
        sender_device: device,
        person: actor.person,
    };
    context
        .verify(&verification_key, &bytes)
        .map_err(|_| ApiError::bad_request("invalid_page"))?;
    // `authenticate` holds the write lock, so nobody saves between this read and the write.
    let stored: Option<i64> =
        query_scalar("SELECT version FROM page_documents WHERE channel_id = ?")
            .bind(&channel)
            .fetch_optional(&mut *tx)
            .await?;
    if stored.unwrap_or(0) != base {
        return Err(keys::conflict("page_changed"));
    }
    let keep = page_files::check(&mut tx, &channel, keep, actor.at).await?;
    messages::remember_sender(&mut tx, &actor).await?;
    query(
        "INSERT INTO page_documents (channel_id, version, epoch, sender_device, sender_person, at, size, envelope)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (channel_id) DO UPDATE SET version = excluded.version, epoch = excluded.epoch,
           sender_device = excluded.sender_device, sender_person = excluded.sender_person,
           at = excluded.at, size = excluded.size, envelope = excluded.envelope",
    )
    .bind(&channel)
    .bind(version)
    .bind(epoch)
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .bind(actor.at)
    .bind(bytes.len() as i64)
    .bind(&bytes)
    .execute(&mut *tx)
    .await?;
    page_files::record(&mut tx, &channel, &keep).await?;
    page_files::refresh(&app, &mut tx, &channel).await?;
    tx.commit().await?;
    app.notify(Event::MessageCreated {
        channel,
        // No message has this ID; apps from before documents look it up and find none.
        id: encode(&store::hash(&bytes)),
        page: true,
    });
    Ok(Json(PageRecord {
        version,
        epoch,
        person: actor.person,
        sender_device: device,
        at: actor.at,
        envelope,
    }))
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::Signer;
    use rusttalk_core::{
        identity::DeviceIdentity,
        pages::{MAX_PAGE_BYTES, MIN_PAGE_BYTES},
    };

    use super::*;
    use crate::{
        Change,
        testing::{app, as_person},
    };

    // Sessions from `fixture`; person 1 is `testing::app`'s own.
    const OWNER: u8 = 11;
    const EDITOR: u8 = 12;
    const READER: u8 = 13;
    const OUTSIDER: u8 = 14;
    const PAGE: [u8; 32] = [20; 32];
    const TEXT: [u8; 32] = [21; 32];

    /// A page and a text channel, where `OWNER` owns the server, `EDITOR` is a moderator,
    /// `READER` may only read and `OUTSIDER` may not read. Each signs in with `as_person`.
    async fn fixture(dir: &std::path::Path) -> anyhow::Result<(App, Vec<DeviceIdentity>)> {
        let app = app(dir, 1).await?;
        query("INSERT INTO categories VALUES ('category', 0, X'00')")
            .execute(&app.db)
            .await?;
        for (id, page) in [(PAGE, true), (TEXT, false)] {
            let id = encode(&id);
            query("INSERT INTO channels (id, kind, page, category_id, position, name_ct) VALUES (?, 'text', ?, 'category', 0, X'00')")
                .bind(&id).bind(page).execute(&app.db).await?;
            for (role, flags) in [("moderator", 3), ("member", 1)] {
                query("INSERT INTO channel_permissions VALUES (?, ?, ?)")
                    .bind(&id)
                    .bind(role)
                    .bind(flags)
                    .execute(&app.db)
                    .await?;
            }
            query("INSERT INTO channel_permissions VALUES (?, 'owner', 3)")
                .bind(&id)
                .execute(&app.db)
                .await?;
            query("INSERT INTO key_scopes (id, kind, channel_id) VALUES (?, 'channel', ?)")
                .bind(&id)
                .bind(&id)
                .execute(&app.db)
                .await?;
            query(
                "INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))",
            )
            .bind(&id)
            .execute(&app.db)
            .await?;
        }
        let mut people = Vec::new();
        for (byte, role) in [
            (OWNER, None),
            (EDITOR, Some("moderator")),
            (READER, Some("member")),
            (OUTSIDER, None),
        ] {
            let (_, identity) = DeviceIdentity::create("device")?;
            let person = identity.certificate.person_id().to_vec();
            let device = identity.certificate.signing_key().to_bytes().to_vec();
            query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
                .bind(&person)
                .execute(&app.db)
                .await?;
            query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
                .bind(store::hash(&[byte; 32]).as_slice()).bind(&person).bind(&device)
                .bind(i64::MAX).bind(vec![byte]).execute(&app.db).await?;
            query("INSERT INTO member_devices VALUES (?, ?, X'03')")
                .bind(&device)
                .bind(&person)
                .execute(&app.db)
                .await?;
            if byte == OWNER {
                query("UPDATE server_settings SET owner_person_id = ?")
                    .bind(&person)
                    .execute(&app.db)
                    .await?;
            }
            if let Some(role) = role {
                query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
                    .bind(&person)
                    .bind(role)
                    .execute(&app.db)
                    .await?;
            }
            people.push(identity);
        }
        Ok((app, people))
    }

    /// The API never decrypts, so a signature over zeros stands in for a sealed page.
    /// Spelled out from docs/crypto.md, as another client would build it.
    fn signed(
        app: &App,
        editor: &DeviceIdentity,
        channel: [u8; 32],
        (base, version, epoch): (i64, i64, i64),
        size: usize,
    ) -> SavePage {
        let mut envelope = vec![0; size - 64];
        envelope[0] = 1;
        let preimage = [
            b"rusttalk-page-v1".as_slice(),
            &keys::space(app).id,
            &channel,
            &epoch.to_be_bytes(),
            &version.to_be_bytes(),
            editor.certificate.signing_key().as_bytes(),
            editor.certificate.person_id(),
            &envelope,
        ]
        .concat();
        envelope.extend_from_slice(&editor.signing_key.sign(&preimage).to_bytes());
        SavePage {
            base,
            version,
            epoch,
            envelope: encode(&envelope),
            keep: Vec::new(),
        }
    }

    async fn save(
        app: &App,
        person: u8,
        channel: [u8; 32],
        page: SavePage,
    ) -> Result<PageRecord, &'static str> {
        put(
            State(app.clone()),
            as_person(person),
            Path(encode(&channel)),
            Ok(Json(page)),
        )
        .await
        .map(|Json(record)| record)
        .map_err(|error| error.code)
    }

    #[tokio::test]
    async fn writers_replace_the_version_they_based_their_change_on() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, people) = fixture(dir.path()).await?;
        let mut events = app.changes.subscribe();

        let page = signed(&app, &people[2], PAGE, (0, 1, 1), MIN_PAGE_BYTES);
        assert_eq!(
            save(&app, READER, PAGE, page).await.err(),
            Some("channel_write_denied")
        );
        let page = signed(&app, &people[3], PAGE, (0, 1, 1), MIN_PAGE_BYTES);
        assert_eq!(
            save(&app, OUTSIDER, PAGE, page).await.err(),
            Some("channel_read_denied")
        );
        let page = signed(&app, &people[0], TEXT, (0, 1, 1), MIN_PAGE_BYTES);
        assert_eq!(
            save(&app, OWNER, TEXT, page).await.err(),
            Some("page_not_found")
        );
        let page = signed(&app, &people[0], PAGE, (0, 1, 2), MIN_PAGE_BYTES);
        assert_eq!(
            save(&app, OWNER, PAGE, page).await.err(),
            Some("epoch_changed")
        );
        let page = signed(&app, &people[0], PAGE, (0, i64::MAX, 1), MIN_PAGE_BYTES);
        assert_eq!(
            save(&app, OWNER, PAGE, page).await.err(),
            Some("invalid_page_version")
        );
        let page = signed(&app, &people[0], PAGE, (0, 1, 1), MIN_PAGE_BYTES);
        assert_eq!(
            save(&app, EDITOR, PAGE, page).await.err(),
            Some("invalid_page")
        );
        let mut page = signed(&app, &people[0], PAGE, (0, 1, 1), MIN_PAGE_BYTES);
        page.keep = vec!["not an ID".into()];
        assert_eq!(
            save(&app, OWNER, PAGE, page).await.err(),
            Some("invalid_keep")
        );
        // Keeping needs write, as saving does.
        let mut page = signed(&app, &people[2], PAGE, (0, 1, 1), MIN_PAGE_BYTES);
        page.keep = vec![encode(&[5; 32])];
        assert_eq!(
            save(&app, READER, PAGE, page).await.err(),
            Some("channel_write_denied")
        );
        assert!(events.try_recv().is_err(), "refusals change nothing");

        let first = signed(&app, &people[0], PAGE, (0, 1, 1), MAX_PAGE_BYTES);
        assert_eq!(
            save(&app, OWNER, PAGE, first)
                .await
                .map(|saved| saved.version),
            Ok(1)
        );
        assert!(matches!(
            events.try_recv(),
            Ok(Change::Update(rusttalk_core::presence::Update::Event(
                Event::MessageCreated { page: true, .. }
            )))
        ));

        let late = signed(&app, &people[1], PAGE, (0, 1, 1), MIN_PAGE_BYTES);
        assert_eq!(
            save(&app, EDITOR, PAGE, late).await.err(),
            Some("page_changed")
        );
        let second = signed(&app, &people[1], PAGE, (1, 2, 1), MIN_PAGE_BYTES);
        let envelope = second.envelope.clone();
        assert_eq!(
            save(&app, EDITOR, PAGE, second)
                .await
                .map(|saved| saved.version),
            Ok(2)
        );

        let read = get(State(app.clone()), as_person(READER), Path(encode(&PAGE)))
            .await
            .map_err(|error| anyhow::anyhow!(error.code))?;
        let read = axum::body::to_bytes(read.into_body(), usize::MAX).await?;
        let read: PageRecord = serde_json::from_slice(&read)?;
        assert!(read.version == 2 && read.envelope == envelope);
        let denied = get(State(app.clone()), as_person(OUTSIDER), Path(encode(&PAGE))).await;
        assert_eq!(
            denied.err().map(|error| error.code),
            Some("channel_read_denied")
        );

        // The last editor's certificate stays fetchable after they left.
        let editor = people[1].certificate.person_id();
        let device = people[1].certificate.signing_key().to_bytes();
        query("DELETE FROM sessions WHERE person_id = ?")
            .bind(editor.as_slice())
            .execute(&app.db)
            .await?;
        query("DELETE FROM members WHERE person_id = ?")
            .bind(editor.as_slice())
            .execute(&app.db)
            .await?;
        let certificate = messages::device(
            State(app.clone()),
            as_person(READER),
            Path((encode(&PAGE), encode(&device), encode(editor))),
        )
        .await;
        assert!(certificate.is_ok());

        query("DELETE FROM channels WHERE id = ?")
            .bind(encode(&PAGE))
            .execute(&app.db)
            .await?;
        let left: i64 = query_scalar("SELECT COUNT(*) FROM page_documents")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(left, 0, "deleting the channel deletes its document");
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_page_channel_has_no_document() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, _) = fixture(dir.path()).await?;
        let read = get(State(app.clone()), as_person(READER), Path(encode(&PAGE)))
            .await
            .map_err(|error| anyhow::anyhow!(error.code))?;
        assert_eq!(read.status(), StatusCode::NO_CONTENT);
        Ok(())
    }

    #[tokio::test]
    async fn page_channels_refuse_messages() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, _) = fixture(dir.path()).await?;
        let post = serde_json::from_value(serde_json::json!({
            "id": encode(&[9; 32]), "epoch": 1, "envelope": encode(&[0; 4]),
        }))?;
        let refused = messages::create(
            State(app.clone()),
            as_person(OWNER),
            Path(encode(&PAGE)),
            Ok(Json(post)),
        )
        .await;
        assert_eq!(refused.err().map(|error| error.code), Some("page_channel"));
        Ok(())
    }

    #[test]
    fn the_server_takes_both_formats_and_no_other() {
        let (_, editor) = DeviceIdentity::create("editor").unwrap();
        let context = PageContext {
            server_id: [1; 32],
            channel_id: PAGE,
            epoch: 1,
            version: 1,
            sender_device: editor.certificate.signing_key().to_bytes(),
            person: *editor.certificate.person_id(),
        };
        for format in 0..=3 {
            let mut envelope = vec![0; MIN_PAGE_BYTES - 64];
            envelope[0] = format;
            let preimage = [
                b"rusttalk-page-v1".as_slice(),
                &context.server_id,
                &context.channel_id,
                &context.epoch.to_be_bytes(),
                &context.version.to_be_bytes(),
                &context.sender_device,
                &context.person,
                &envelope,
            ]
            .concat();
            envelope.extend_from_slice(&editor.signing_key.sign(&preimage).to_bytes());
            let taken = context.verify(editor.certificate.signing_key(), &envelope);
            assert_eq!(taken.is_ok(), matches!(format, 1 | 2), "format {format}");
        }
    }
}
