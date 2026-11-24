use super::*;
use crate::testing::person;
use reqwest::{Method, RequestBuilder};
use serde_json::{Value, json};
use sqlx::{
    Connection, SqliteConnection,
    migrate::{Migrate, Migration, MigrationType},
    sqlite::SqliteConnectOptions,
};

// Use a client-chosen role ID so the test can delete and recreate it through the API.
const LEAD: &str = "CQkJCQkJCQkJCQkJCQkJCQ";

async fn before_events() -> anyhow::Result<(tempfile::TempDir, SqliteConnection)> {
    let dir = tempfile::tempdir()?;
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("rusttalk.sqlite3"))
        .create_if_missing(true);
    let mut old = sqlx::SqliteConnection::connect_with(&options).await?;
    old.ensure_migrations_table().await?;
    for migration in sqlx::migrate!().iter().take_while(|m| m.version < 34) {
        old.apply(migration).await?;
    }
    query(
        "INSERT INTO roles (id, kind, rank, permissions) VALUES
           ('plain', 'custom', 5, 1), ('helper', 'custom', 10, 9),
           ('manager', 'custom', 60, 8), ('staff', 'custom', 55, 3110),
           (?, 'custom', 54, 3118), ('junior', 'custom', 12, 3118),
           ('low-manager', 'custom', 25, 8), ('low-staff', 'custom', 20, 3110),
           ('peer', 'custom', 60, 3118)",
    )
    .bind(LEAD)
    .execute(&mut old)
    .await?;
    for (who, role) in [
        (2, "manager"),
        (3, "manager"),
        (4, "member"),
        (5, "low-manager"),
    ] {
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
            .bind(person(who))
            .execute(&mut old)
            .await?;
        query("INSERT INTO member_roles VALUES (?, ?)")
            .bind(person(who))
            .bind(role)
            .execute(&mut old)
            .await?;
        query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
            .bind(store::hash(&[who; 32]).as_slice()).bind(person(who)).bind(vec![who]).bind(i64::MAX).bind(vec![who]).execute(&mut old).await?;
    }
    query("INSERT INTO member_roles VALUES (?, 'staff')")
        .bind(person(2))
        .execute(&mut old)
        .await?;
    query("INSERT INTO member_roles VALUES (?, 'low-staff')")
        .bind(person(5))
        .execute(&mut old)
        .await?;
    Ok((dir, old))
}

async fn check(request: RequestBuilder, status: StatusCode, error: &str) -> anyhow::Result<()> {
    let response = request.send().await?;
    let actual = response.status();
    let body = response.text().await?;
    assert_eq!(actual, status, "{body}");
    if !error.is_empty() {
        assert_eq!(serde_json::from_str::<Value>(&body)?["error"], error);
    }
    Ok(())
}

#[tokio::test]
async fn upgrade_preserves_only_existing_role_operations() -> anyhow::Result<()> {
    let (dir, old) = before_events().await?;
    old.close().await?;
    let app = testing::app(dir.path(), 1).await?;
    let db = &app.db;
    let permissions: std::collections::HashMap<String, i64> =
        // Without `start_focus` (32768), which a later migration gives every role.
        query_as::<_, (String, i64)>("SELECT id, permissions & ~32768 FROM roles")
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    assert_eq!(permissions["plain"], 1);
    assert_eq!(permissions["member"], 4096);
    for (who, target) in [
        ("helper", "member"),
        (LEAD, "moderator"),
        ("admin", "moderator"),
    ] {
        assert_eq!(
            permissions[target] & !permissions[who],
            0,
            "{who} grants {target}"
        );
    }

    let mut tx = store::write(db).await?;
    keys::init_space(&mut tx, &keys::space(&app).id).await?;
    query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
        .bind(encode(&keys::space(&app).id))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = crate::routes(app.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let http = reqwest::Client::new();
    let request = |who, method, path: &str| {
        http.request(method, format!("http://{address}/v1/{path}"))
            .headers(as_person(who))
    };
    let assign = |who, recipient, roles: &[&str]| {
        request(
            who,
            Method::PUT,
            &format!("members/{}/roles", encode(&person(recipient))),
        )
        .json(&json!({"role_ids": roles}))
    };
    let sealed = encode(&[&1i64.to_be_bytes()[..], &[7; 40]].concat());
    let rename = |who, id| {
        request(who, Method::PUT, &format!("roles/{id}/name"))
            .json(&json!({"display_ct": sealed, "name_ct": sealed}))
    };
    let edit = |who, id, flags: &[&str]| {
        request(who, Method::PUT, &format!("roles/{id}")).json(&json!({"permissions": flags}))
    };
    let delete = |who, id| request(who, Method::DELETE, &format!("roles/{id}"));
    let default = |id| request(2, Method::PUT, "roles/default").json(&json!({"role_id":id}));
    let ok = StatusCode::NO_CONTENT;
    let denied = StatusCode::FORBIDDEN;
    let missing = "cannot_grant_permission";

    // Rank 25 can still manage its old rank 12 target, despite never outranking Moderator.
    check(assign(5, 4, &["junior"]), ok, "").await?;
    check(assign(2, 4, &[LEAD]), ok, "").await?;
    check(default(LEAD), ok, "").await?;
    check(rename(2, "moderator"), ok, "").await?;
    check(rename(2, LEAD), ok, "").await?;
    let auth = crate::roles::authority(&mut *db.acquire().await?, &person(2)).await?;
    assert!(!auth.has(crate::roles::Permission::CreateEvents));
    assert!(!auth.has(crate::roles::Permission::ManageEvents));

    // Later assignments, a newly lowered peer, and an unrelated edited role confer no exemption.
    check(assign(1, 3, &["manager", "staff"]), ok, "").await?;
    check(assign(3, 4, &[LEAD]), denied, missing).await?;
    check(rename(3, "moderator"), denied, missing).await?;
    let flags = ["suggest_events", "create_events", "manage_events"];
    let lower =
        request(1, Method::PUT, "roles/peer").json(&json!({"rank": 30, "permissions": flags}));
    check(lower, ok, "").await?;
    check(rename(2, "peer"), denied, missing).await?;
    check(edit(1, "plain", &flags), ok, "").await?;
    check(default("plain"), denied, missing).await?;

    // Current rank and pre-calendar rights are both still required.
    check(assign(1, 2, &["low-manager", "low-staff"]), ok, "").await?;
    check(rename(2, LEAD), denied, "role_hierarchy").await?;
    check(assign(1, 2, &["manager"]), ok, "").await?;
    check(rename(2, "moderator"), denied, missing).await?;
    check(edit(2, LEAD, &flags), denied, missing).await?;
    check(assign(1, 2, &["manager", "staff"]), ok, "").await?;

    check(assign(2, 4, &["moderator"]), ok, "").await?;
    check(default("member"), ok, "").await?;
    check(edit(2, LEAD, &flags), ok, "").await?;
    check(delete(2, LEAD), ok, "").await?;
    let new_role =
        request(1, Method::POST, "roles").json(&json!({"id": LEAD, "rank":54,"permissions":flags}));
    check(new_role, StatusCode::CREATED, "").await?;
    check(assign(2, 4, &[LEAD]), denied, missing).await?;
    check(delete(2, LEAD), denied, missing).await?;

    // Keeping existing missing bits is permitted. Removing one forfeits it even if
    // the owner adds it back, and unrelated permission additions need direct authority.
    check(
        edit(2, "junior", &["manage_server", "manage_events"]),
        denied,
        missing,
    )
    .await?;
    check(edit(2, "junior", &flags), ok, "").await?;
    check(edit(2, "junior", &flags[..2]), ok, "").await?;
    check(edit(2, "junior", &flags), denied, missing).await?;
    check(edit(1, "junior", &flags), ok, "").await?;
    check(rename(2, "junior"), denied, missing).await?;
    check(edit(2, "junior", &[]), denied, missing).await?;
    Ok(())
}

#[tokio::test]
async fn preview_upgrade_recognizes_only_the_original_checksum_and_recorded_grants()
-> anyhow::Result<()> {
    let (dir, mut old) = before_events().await?;
    old.apply(sqlx::migrate!().iter().find(|m| m.version == 34).unwrap())
        .await?;
    let preview = Migration::new(
        35,
        "calendar delegation".into(),
        MigrationType::Simple,
        include_str!("../../../preview/0035_calendar_delegation.sql").into(),
        false,
    );
    old.apply(&preview).await?;
    // A different 0035 belongs to another migration history. Leave it intact.
    query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 35")
        .execute(&mut old)
        .await?;
    let error = store::open(dir.path()).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref(),
        Some(
            sqlx::migrate::MigrateError::VersionMissing(35)
                | sqlx::migrate::MigrateError::VersionMismatch(35)
        )
    ));
    let checksum: Vec<u8> =
        query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 35")
            .fetch_one(&mut old)
            .await?;
    assert_eq!(checksum, [0]);
    query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 35")
        .bind(preview.checksum.as_ref())
        .execute(&mut old)
        .await?;
    old.close().await?;
    for _ in 0..2 {
        let db = store::open(dir.path()).await?;
        let grants: Vec<(Vec<u8>, String)> =
            query_as("SELECT person_id, role_id FROM calendar_role_grants")
                .fetch_all(&db)
                .await?;
        assert_eq!(grants, [(person(2), "moderator".into())]);
        let versions: Vec<i64> = query_scalar(
            "SELECT version FROM _sqlx_migrations WHERE version IN (34, 35, 37) ORDER BY version",
        )
        .fetch_all(&db)
        .await?;
        assert_eq!(versions, [34, 35, 37]);
        db.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn upgrade_from_either_feature_branch_preserves_calendar_data() -> anyhow::Result<()> {
    for existing in [35, 37] {
        let (dir, mut old) = before_events().await?;
        for migration in sqlx::migrate!()
            .iter()
            .filter(|m| m.version == 34 || m.version == existing)
        {
            old.apply(migration).await?;
        }
        query("INSERT INTO channel_ids VALUES ('event')")
            .execute(&mut old)
            .await?;
        query("INSERT INTO calendar_events (id, author, author_device, certificate, approved, epoch, remove_at, envelope) VALUES ('event', X'01', X'02', X'03', 1, 1, 123, X'04')")
            .execute(&mut old).await?;
        if existing == 35 {
            query("UPDATE calendar_events SET reminded_at = 42")
                .execute(&mut old)
                .await?;
            query("INSERT INTO calendar_answers VALUES ('event', X'05', X'06', X'07', 2, X'08')")
                .execute(&mut old)
                .await?;
        }
        old.close().await?;
        let db = store::open(dir.path()).await?;
        let reminder: Option<i64> =
            query_scalar("SELECT reminded_at FROM calendar_events WHERE id = 'event'")
                .fetch_one(&db)
                .await?;
        assert_eq!(reminder, (existing == 35).then_some(42));
        let answers: Vec<(i64, Vec<u8>)> =
            query_as("SELECT epoch, envelope FROM calendar_answers WHERE event_id = 'event'")
                .fetch_all(&db)
                .await?;
        if existing == 35 {
            assert_eq!(answers, [(2, vec![8])]);
        } else {
            assert!(answers.is_empty());
        }
        let versions: Vec<i64> = query_scalar(
            "SELECT version FROM _sqlx_migrations WHERE version IN (34, 35, 37) ORDER BY version",
        )
        .fetch_all(&db)
        .await?;
        assert_eq!(versions, [34, 35, 37]);
        db.close().await;
    }
    Ok(())
}
