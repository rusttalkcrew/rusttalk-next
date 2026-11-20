use std::{net::IpAddr, time::Duration};

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
};
use ed25519_dalek::Signature;
use rusttalk_core::{
    auth::{Challenge, Session, SignIn, auth_payload, decode, decode_bytes, encode},
    identity::{DeviceCertificate, PersonId, Revocation},
};
use serde::Deserialize;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App, keys, payload,
    store::{audit, hash, log_closed, now, random, write},
};

const PUBLIC_LIMIT: i64 = 256;
// Several friends behind one home router reconnect at once after a restart.
const PUBLIC_LIMIT_PER_IP: i64 = 32;
const SESSIONS_PER_PERSON: i64 = 16;
const REVOCATIONS_PER_PERSON: i64 = 64;
const CHALLENGE_TTL: i64 = 60;
const SESSION_TTL: i64 = Duration::from_hours(12).as_secs() as i64;

pub async fn challenge(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Challenge>> {
    let ip = client_ip(&headers)?;
    let mut tx = write(&app.db).await?;
    let at = now()?;
    let ip = check_public_limit(&mut tx, &ip, at, PublicLimit::Challenge).await?;
    let challenge = random::<32>()?;
    query("INSERT INTO challenges (nonce, ip, expires_at) VALUES (?, ?, ?)")
        .bind(challenge.as_slice())
        .bind(&ip)
        .bind(at + CHALLENGE_TTL)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(Challenge::signed(&challenge, &app.key)))
}

async fn consume_challenge(db: &SqlitePool, challenge: &[u8; 32], at: i64) -> ApiResult<()> {
    let result = query("DELETE FROM challenges WHERE nonce = ? AND expires_at > ?")
        .bind(challenge.as_slice())
        .bind(at)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::unauthorized("invalid_challenge"));
    }
    Ok(())
}

pub async fn sign_in(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<SignIn>, JsonRejection>,
) -> ApiResult<Json<Session>> {
    let request = payload(body)?;
    let ip = client_ip(&headers)?;
    let challenge = decode(&request.challenge)?;
    // Commit consumption even if subsequent checks reject the sign-in.
    consume_challenge(&app.db, &challenge, now()?).await?;
    let server_id = app.key.verifying_key().to_bytes();
    let cert = verify_device(&request, &challenge, &server_id)?;
    let person = cert.person_id().as_slice();
    let device = cert.signing_key().as_bytes().as_slice();

    let mut tx = write(&app.db).await?;
    let at = now()?;
    check_revoked(&mut tx, person, device).await?;
    let banned: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM bans WHERE person_id = ?)")
        .bind(person)
        .fetch_one(&mut *tx)
        .await?;
    if banned {
        return Err(ApiError::forbidden("person_banned"));
    }
    check_ip(&mut tx, &ip, person).await?;
    let member: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ?)")
        .bind(person)
        .fetch_one(&mut *tx)
        .await?;
    if !member {
        let invite = request
            .invite
            .as_deref()
            .ok_or(ApiError::forbidden("invite_required"))?;
        redeem(&mut tx, person, invite, at).await?;
    }

    keys::init_space(&mut tx, &server_id).await?;
    let added = keys::register_device(&mut tx, &cert).await?;
    let closed = remove_device_sessions(&mut tx, person, device).await?;
    let session = issue_session(&mut tx, person, device, &ip, at).await?;
    tx.commit().await?;
    log_closed(closed, at, "replaced");
    if added {
        keys::changed(&app, person, Some(device));
    } else {
        app.notify(None);
    }
    Ok(Json(session))
}

fn verify_device(
    request: &SignIn,
    challenge: &[u8; 32],
    server_id: &[u8; 32],
) -> ApiResult<DeviceCertificate> {
    let cert = DeviceCertificate::verify_public(
        decode(&request.person_id)?,
        &decode_bytes(&request.certificate)?,
    )
    .map_err(|_| ApiError::unauthorized("invalid_certificate"))?;
    cert.signing_key()
        .verify_strict(
            &auth_payload(challenge, server_id),
            &Signature::from_bytes(&decode(&request.signature)?),
        )
        .map_err(|_| ApiError::unauthorized("invalid_device_signature"))?;
    Ok(cert)
}

async fn check_revoked(db: &mut SqliteConnection, person: &[u8], device: &[u8]) -> ApiResult<()> {
    let revoked: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM revocations WHERE person_id = ? AND device_id = ?)",
    )
    .bind(person)
    .bind(device)
    .fetch_one(db)
    .await?;
    if revoked {
        return Err(ApiError::forbidden("device_revoked"));
    }
    Ok(())
}

pub fn client_ip(headers: &HeaderMap) -> ApiResult<String> {
    let mut values = headers.get_all("x-forwarded-for").iter();
    let ip = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<IpAddr>().ok())
        .filter(|_| values.next().is_none())
        .ok_or(ApiError::bad_request("invalid_forwarded_ip"))?;
    Ok(ip.to_canonical().to_string())
}

/// One IPv6 user owns a whole /64, so rate limits count that prefix as one address.
fn limit_key(ip: &str) -> String {
    match ip.parse() {
        Ok(IpAddr::V6(v6)) => {
            std::net::Ipv6Addr::from(u128::from(v6) & !(u128::MAX >> 64)).to_string()
        }
        _ => ip.to_owned(),
    }
}

pub enum PublicLimit {
    Challenge,
    InvitePreview,
}

pub async fn check_public_limit(
    db: &mut SqliteConnection,
    ip: &str,
    at: i64,
    limit: PublicLimit,
) -> ApiResult<String> {
    let (table, code) = match limit {
        PublicLimit::Challenge => ("challenges", "challenge_limit"),
        PublicLimit::InvitePreview => ("invite_previews", "invite_preview_limit"),
    };
    let ip = limit_key(ip);
    query(&format!("DELETE FROM {table} WHERE expires_at <= ?"))
        .bind(at)
        .execute(&mut *db)
        .await?;
    let (total, from_ip): (i64, i64) = query_as(&format!(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE ip = ?) FROM {table}"
    ))
    .bind(&ip)
    .fetch_one(db)
    .await?;
    if total >= PUBLIC_LIMIT || from_ip >= PUBLIC_LIMIT_PER_IP {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, code));
    }
    Ok(ip)
}

async fn check_ip(db: &mut SqliteConnection, ip: &str, person: &[u8]) -> ApiResult<()> {
    let banned: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM ban_ips WHERE ip = ?)
         AND NOT EXISTS (SELECT 1 FROM server_settings WHERE owner_person_id = ?)",
    )
    .bind(ip)
    .bind(person)
    .fetch_one(db)
    .await?;
    if banned {
        return Err(ApiError::forbidden("ip_banned"));
    }
    Ok(())
}

pub async fn owner_pending(db: &SqlitePool) -> Result<bool, sqlx::Error> {
    query_scalar("SELECT owner_person_id IS NULL FROM server_settings WHERE id = 1")
        .fetch_one(db)
        .await
}

pub async fn owner_invite(db: &SqlitePool) -> ApiResult<Option<String>> {
    let mut tx = write(db).await?;
    let pending: bool =
        query_scalar("SELECT owner_person_id IS NULL FROM server_settings WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    if !pending {
        return Ok(None);
    }
    query("DELETE FROM invites WHERE is_owner = TRUE")
        .execute(&mut *tx)
        .await?;
    let (_, code) = create_invite(&mut tx, true, 1, now()? + 86_400).await?;
    tx.commit().await?;
    Ok(Some(code))
}

pub async fn create_invite(
    db: &mut SqliteConnection,
    owner: bool,
    uses: i64,
    expiry: i64,
) -> ApiResult<(String, String)> {
    let id = encode(&random::<16>()?);
    let code = random::<16>()?;
    query(
        "INSERT INTO invites (id, code_hash, is_owner, remaining_uses, expires_at)
           VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(hash(&code).as_slice())
    .bind(owner)
    .bind(uses)
    .bind(expiry)
    .execute(db)
    .await?;
    Ok((id, encode(&code)))
}

async fn redeem(db: &mut SqliteConnection, person: &[u8], invite: &str, at: i64) -> ApiResult<()> {
    let code = decode::<16>(invite)?;
    let (id, owner, remaining): (String, bool, i64) = query_as(
        "SELECT id, is_owner, remaining_uses FROM invites WHERE code_hash = ? AND expires_at > ?",
    )
    .bind(hash(&code).as_slice())
    .bind(at)
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::forbidden("invalid_invite"))?;
    query("INSERT INTO members (person_id, joined_at) VALUES (?, ?)")
        .bind(person)
        .bind(at)
        .execute(&mut *db)
        .await?;
    query(
        "INSERT INTO member_roles (person_id, role_id)
           SELECT ?, default_role_id FROM server_settings WHERE id = 1",
    )
    .bind(person)
    .execute(&mut *db)
    .await?;
    if owner {
        let claimed = query("UPDATE server_settings SET owner_person_id = ? WHERE id = 1 AND owner_person_id IS NULL")
            .bind(person).execute(&mut *db).await?;
        if claimed.rows_affected() == 0 {
            return Err(ApiError::forbidden("invalid_invite"));
        }
    }
    if remaining == 1 {
        query("DELETE FROM invites WHERE id = ?")
            .bind(&id)
            .execute(&mut *db)
            .await?;
    } else {
        query("UPDATE invites SET remaining_uses = remaining_uses - 1 WHERE id = ?")
            .bind(&id)
            .execute(&mut *db)
            .await?;
    }
    audit(
        db,
        person,
        if owner { "owner_claimed" } else { "joined" },
        Some(person),
        Some(&id),
        at,
    )
    .await?;
    Ok(())
}

async fn issue_session(
    db: &mut SqliteConnection,
    person: &[u8],
    device: &[u8],
    ip: &str,
    at: i64,
) -> ApiResult<Session> {
    let token = random::<32>()?;
    let expires_at = at + SESSION_TTL;
    // Device certificates are free to mint, so bound sessions per person by dropping the oldest.
    let dropped: Vec<i64> = query_scalar(
        "DELETE FROM sessions WHERE person_id = ? AND token_hash NOT IN (SELECT token_hash
         FROM sessions WHERE person_id = ? ORDER BY expires_at DESC LIMIT ?) RETURNING expires_at",
    )
    .bind(person)
    .bind(person)
    .bind(SESSIONS_PER_PERSON - 1)
    .fetch_all(&mut *db)
    .await?;
    log_closed(dropped, at, "session_limit");
    query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at) VALUES (?, ?, ?, ?, ?)")
        .bind(hash(&token).as_slice()).bind(person).bind(device).bind(ip).bind(expires_at)
        .execute(db).await?;
    Ok(Session {
        token: encode(&token),
        expires_at,
    })
}

async fn remove_device_sessions(
    db: &mut SqliteConnection,
    person: &[u8],
    device: &[u8],
) -> ApiResult<Vec<i64>> {
    Ok(query_scalar(
        "DELETE FROM sessions WHERE person_id = ? AND device_id = ? RETURNING expires_at",
    )
    .bind(person)
    .bind(device)
    .fetch_all(db)
    .await?)
}

pub struct Actor {
    pub person: PersonId,
    pub device: Vec<u8>,
    pub ip: String,
    pub at: i64,
    pub token_hash: [u8; 32],
    pub expires_at: i64,
}

pub async fn authenticate(
    app: &App,
    headers: &HeaderMap,
) -> ApiResult<(Transaction<'static, Sqlite>, Actor)> {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|value| decode::<32>(value).ok())
        .ok_or(ApiError::unauthorized("invalid_session"))?;
    let ip = client_ip(headers)?;
    let token_hash = hash(&token);
    let mut tx = write(&app.db).await?;
    let at = now()?;
    let (person, device, expires_at): (Vec<u8>, Vec<u8>, i64) =
        query_as("SELECT person_id, device_id, expires_at FROM sessions WHERE token_hash = ?")
            .bind(token_hash.as_slice())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::unauthorized("invalid_session"))?;
    if expires_at <= at {
        query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash.as_slice())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        log_closed(vec![expires_at], at, "token_expired");
        return Err(ApiError::unauthorized("token_expired"));
    }
    check_ip(&mut tx, &ip, &person).await?;
    let person = person
        .try_into()
        .map_err(|_| ApiError::internal("invalid_stored_person"))?;
    Ok((
        tx,
        Actor {
            person,
            device,
            ip,
            at,
            token_hash,
            expires_at,
        },
    ))
}

pub async fn refresh(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Session>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let closed = remove_device_sessions(&mut tx, &actor.person, &actor.device).await?;
    let session = issue_session(&mut tx, &actor.person, &actor.device, &actor.ip, actor.at).await?;
    tx.commit().await?;
    log_closed(closed, actor.at, "refreshed");
    app.notify(None);
    Ok(Json(session))
}

#[derive(Deserialize)]
pub struct RevocationBody {
    record: String,
}

pub async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<RevocationBody>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let bytes = decode_bytes(&payload(body)?.record)?;
    let revocation = Revocation::verify(actor.person, &bytes)
        .map_err(|_| ApiError::bad_request("invalid_revocation"))?;
    let device = revocation.device_signing_key().as_slice();
    let stored: i64 = query_scalar("SELECT COUNT(*) FROM revocations WHERE person_id = ?")
        .bind(actor.person.as_slice())
        .fetch_one(&mut *tx)
        .await?;
    if stored >= REVOCATIONS_PER_PERSON {
        return Err(ApiError::new(StatusCode::CONFLICT, "revocation_limit"));
    }
    let added = query(
        "INSERT INTO revocations (person_id, device_id, record) VALUES (?, ?, ?)
                       ON CONFLICT (person_id, device_id) DO NOTHING",
    )
    .bind(actor.person.as_slice())
    .bind(device)
    .bind(bytes)
    .execute(&mut *tx)
    .await?;
    let closed = remove_device_sessions(&mut tx, &actor.person, device).await?;
    if added.rows_affected() != 0 {
        let removed = query("DELETE FROM member_devices WHERE person_id = ? AND device_id = ?")
            .bind(actor.person.as_slice())
            .bind(device)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if removed != 0 {
            keys::member_removed(&app, &mut tx, &actor.person).await?;
        }
        audit(
            &mut tx,
            &actor.person,
            "device_revoked",
            Some(&actor.person),
            Some(&encode(device)),
            actor.at,
        )
        .await?;
    }
    tx.commit().await?;
    log_closed(closed, actor.at, "revoked");
    keys::changed(&app, &actor.person, Some(device));
    crate::voice::enforce(&app, None);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttalk_core::identity::{DeviceIdentity, revoke_device};

    #[test]
    fn rejects_bad_device_proofs() {
        let (_, identity) = DeviceIdentity::create("test").unwrap();
        let challenge = [3; 32];
        let server_id = [4; 32];
        let mut proof = SignIn::signed(&identity, &challenge, &server_id, None).unwrap();
        let expected = [challenge.as_slice(), &server_id, b"rusttalk-auth-v1"].concat();
        let signature = Signature::from_bytes(&decode(&proof.signature).unwrap());
        identity
            .certificate
            .signing_key()
            .verify_strict(&expected, &signature)
            .unwrap();
        assert!(verify_device(&proof, &challenge, &server_id).is_ok());
        for result in [
            verify_device(&proof, &[0; 32], &server_id),
            verify_device(&proof, &challenge, &[0; 32]),
        ] {
            assert_eq!(result.unwrap_err().code, "invalid_device_signature");
        }
        let mut cert = identity.certificate.public_bytes().unwrap().to_vec();
        cert[30] ^= 1;
        proof.certificate = encode(&cert);
        assert_eq!(
            verify_device(&proof, &challenge, &server_id)
                .unwrap_err()
                .code,
            "invalid_certificate"
        );
    }

    #[tokio::test]
    async fn one_ip_cannot_take_every_challenge() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let app = crate::App {
            files: crate::files::Files::open(dir.path(), &db)
                .await
                .unwrap()
                .into(),
            db,
            key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
            changes: tokio::sync::broadcast::channel(128).0,
            voice: None,
        };
        let from = |ip: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", ip.parse().unwrap());
            headers
        };
        for _ in 0..PUBLIC_LIMIT_PER_IP {
            assert!(
                challenge(State(app.clone()), from("2001:db8:1:2::1"))
                    .await
                    .is_ok()
            );
        }
        let refused = challenge(State(app.clone()), from("2001:db8:1:2::2")).await;
        assert_eq!(refused.err().unwrap().code, "challenge_limit");
        assert!(challenge(State(app), from("2001:db8:1:3::1")).await.is_ok());
        assert_eq!(limit_key("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2::");
        assert_eq!(limit_key("192.0.2.1"), "192.0.2.1");
    }

    #[tokio::test]
    async fn ip_bans_block_members_but_never_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let mut conn = db.acquire().await.unwrap();
        for sql in [
            "INSERT INTO members (person_id, joined_at) VALUES (x'01', 0), (x'02', 0), (x'03', 0)",
            "UPDATE server_settings SET owner_person_id = x'01'",
            "INSERT INTO bans (person_id) VALUES (x'03')",
            "INSERT INTO ban_ips (person_id, ip) VALUES (x'03', '192.0.2.9')",
        ] {
            query(sql).execute(&mut *conn).await.unwrap();
        }
        let banned = check_ip(&mut conn, "192.0.2.9", &[2]).await;
        assert_eq!(banned.unwrap_err().code, "ip_banned");
        assert!(check_ip(&mut conn, "192.0.2.9", &[1]).await.is_ok());
        assert!(check_ip(&mut conn, "192.0.2.8", &[2]).await.is_ok());
    }

    #[tokio::test]
    async fn sessions_per_person_keep_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let mut conn = db.acquire().await.unwrap();
        let person = |id: u8| [id; 32];
        for id in [1, 2] {
            query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
                .bind(person(id).as_slice())
                .execute(&mut *conn)
                .await
                .unwrap();
        }
        for at in 0..=SESSIONS_PER_PERSON {
            issue_session(&mut conn, &[1; 32], &[at as u8; 32], "192.0.2.1", at)
                .await
                .unwrap();
        }
        issue_session(&mut conn, &[2; 32], &[0; 32], "192.0.2.1", 0)
            .await
            .unwrap();
        let rows: Vec<(Vec<u8>, i64)> =
            query_as("SELECT person_id, MIN(expires_at) FROM sessions GROUP BY person_id")
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        let count: i64 = query_scalar("SELECT COUNT(*) FROM sessions WHERE person_id = ?")
            .bind([1u8; 32].as_slice())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(count, SESSIONS_PER_PERSON);
        assert_eq!(
            rows,
            vec![(vec![1; 32], 1 + SESSION_TTL), (vec![2; 32], SESSION_TTL)]
        );
    }

    #[tokio::test]
    async fn challenges_are_single_use_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        for nonce in [[1u8; 32], [2u8; 32]] {
            query("INSERT INTO challenges (nonce, expires_at) VALUES (?, 101)")
                .bind(nonce.as_slice())
                .execute(&db)
                .await
                .unwrap();
        }
        let nonce = [1; 32];
        let (first, second) = tokio::join!(
            consume_challenge(&db, &nonce, 100),
            consume_challenge(&db, &nonce, 100)
        );
        assert_ne!(first.is_ok(), second.is_ok());
        assert_eq!(
            consume_challenge(&db, &[2; 32], 101)
                .await
                .unwrap_err()
                .code,
            "invalid_challenge"
        );
    }

    #[tokio::test]
    async fn stored_revocations_block_only_that_person_and_device() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let (words, identity) = DeviceIdentity::create("test").unwrap();
        let person = identity.certificate.person_id();
        let device = identity.certificate.signing_key().as_bytes();
        let revocation = revoke_device(&words, *device).unwrap();
        let mut conn = db.acquire().await.unwrap();
        assert!(check_revoked(&mut conn, person, device).await.is_ok());
        query("INSERT INTO revocations (person_id, device_id, record) VALUES (?, ?, ?)")
            .bind(person.as_slice())
            .bind(device.as_slice())
            .bind(revocation.to_bytes())
            .execute(&mut *conn)
            .await
            .unwrap();
        assert_eq!(
            check_revoked(&mut conn, person, device)
                .await
                .unwrap_err()
                .code,
            "device_revoked"
        );
        assert!(check_revoked(&mut conn, person, &[7; 32]).await.is_ok());
        assert!(check_revoked(&mut conn, &[8; 32], device).await.is_ok());
    }
}
