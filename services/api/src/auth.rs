use std::{
    env,
    net::IpAddr,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use anyhow::{Context, ensure};

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
    ApiError, ApiResult, App, Change, keys, payload,
    store::{WriteTx, audit, hash, log_closed, now, random, write},
};

const PUBLIC_LIMIT: i64 = 256;
// Several friends behind one home router reconnect at once after a restart.
const PUBLIC_LIMIT_PER_IP: i64 = 32;
const CHALLENGE_LIMIT: usize = 4096;
// About 30 devices behind one router, each with an old client that also fetches an unused
// challenge when it connects.
const CHALLENGE_LIMIT_PER_IP: usize = 64;
const SESSIONS_PER_PERSON: i64 = 16;
use rusttalk_core::identity::REVOCATIONS_PER_PERSON;
const CHALLENGE_TTL: i64 = 60;
pub const SESSION_TTL: i64 = Duration::from_hours(12).as_secs() as i64;

pub fn session_ttl() -> anyhow::Result<i64> {
    if env::var("RUSTTALK_DEV").as_deref() == Ok("1")
        && let Ok(value) = env::var("RUSTTALK_DEV_SESSION_TTL_SECS")
        && !value.is_empty()
    {
        let ttl = value.parse().context("invalid dev session lifetime")?;
        ensure!(
            (10..=SESSION_TTL).contains(&ttl),
            "dev session lifetime out of range"
        );
        return Ok(ttl);
    }
    Ok(SESSION_TTL)
}

/// Outstanding sign-in challenges. They are single-use and live 60 seconds, so they stay in
/// memory: issuing and consuming one takes no writer turn, and a restart only makes a sign-in
/// in progress fetch a new one.
static CHALLENGES: Mutex<Challenges> = Mutex::new(Challenges(Vec::new()));

/// Each challenge with the rate-limit key of the IP that fetched it and its expiry, oldest first.
struct Challenges(Vec<([u8; 32], String, i64)>);

impl Challenges {
    fn issue(&mut self, nonce: [u8; 32], ip: String, at: i64) -> ApiResult<()> {
        self.0.retain(|(_, _, expires_at)| *expires_at > at);
        let mut from_ip = self
            .0
            .iter()
            .enumerate()
            .filter(|(_, (_, key, _))| *key == ip);
        // An IP at its share gives up its oldest challenge, which a reconnect loop or an old
        // client's connect check left unused, so nobody else behind that router is locked out.
        if let Some((oldest, _)) = from_ip.next()
            && from_ip.count() + 1 >= CHALLENGE_LIMIT_PER_IP
        {
            self.0.remove(oldest);
        } else if self.0.len() >= CHALLENGE_LIMIT {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "challenge_limit",
            ));
        }
        self.0.push((nonce, ip, at + CHALLENGE_TTL));
        Ok(())
    }

    /// Removes the challenge, so it is accepted at most once, and only before it expires.
    fn take(&mut self, nonce: &[u8; 32], at: i64) -> ApiResult<()> {
        let invalid = || ApiError::unauthorized("invalid_challenge");
        let index = self.0.iter().position(|(stored, _, _)| stored == nonce);
        let (_, _, expires_at) = self.0.remove(index.ok_or_else(invalid)?);
        if expires_at <= at {
            return Err(invalid());
        }
        Ok(())
    }
}

fn challenges() -> std::sync::MutexGuard<'static, Challenges> {
    // Neither method can panic halfway, so the list is consistent even after a poisoning panic.
    CHALLENGES.lock().unwrap_or_else(PoisonError::into_inner)
}

pub async fn challenge(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Challenge>> {
    let ip = limit_key(&client_ip(&headers)?);
    let challenge = random::<32>()?;
    challenges().issue(challenge, ip, now()?)?;
    Ok(Json(Challenge::signed(&challenge, &app.key)))
}

pub async fn sign_in(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<SignIn>, JsonRejection>,
) -> ApiResult<Json<Session>> {
    sign_in_device(&app, &headers, payload(body)?, false).await
}

pub async fn open_join(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<SignIn>, JsonRejection>,
) -> ApiResult<Json<Session>> {
    let request = payload(body)?;
    if request.invite.is_some() {
        return Err(ApiError::bad_request("unexpected_invite"));
    }
    sign_in_device(&app, &headers, request, true).await
}

async fn sign_in_device(
    app: &App,
    headers: &HeaderMap,
    request: SignIn,
    open_join: bool,
) -> ApiResult<Json<Session>> {
    // Protocol 2 clients cannot read protected epochs.
    if request.protocol < 3 {
        return Err(ApiError::new(
            StatusCode::UPGRADE_REQUIRED,
            "client_update_required",
        ));
    }
    let ip = client_ip(headers)?;
    let challenge = decode(&request.challenge)?;
    // Consume the challenge even if subsequent checks reject the sign-in.
    challenges().take(&challenge, now()?)?;
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
    let mut locked = false;
    if !member {
        if request.invite.is_none() && !open_join {
            return Err(ApiError::forbidden("invite_required"));
        }
        crate::lockdown::check(app, &mut tx).await?;
        if let Some(invite) = request.invite.as_deref() {
            redeem(&mut tx, person, invite, at).await?;
        } else {
            admit_open(&mut tx, person, &ip, at).await?;
        }
        locked = crate::lockdown::joined(app, &mut tx, person, at).await?;
    }

    keys::init_space(&mut tx, &server_id).await?;
    let added = keys::register_device(&mut tx, &cert).await?;
    if member && added {
        audit(
            &mut tx,
            person,
            "device_added",
            Some(person),
            Some(&encode(device)),
            at,
        )
        .await?;
    }
    let closed = remove_device_sessions(&mut tx, person, device).await?;
    let session = issue_session(&mut tx, person, device, &ip, at, app.session_ttl).await?;
    tx.commit().await?;
    log_closed(closed, at, "replaced");
    if locked {
        crate::lockdown::auto_locked(app);
    }
    if added || !member {
        keys::changed(app, person, Some(device));
    } else {
        app.signal(Change::Session(*cert.person_id()));
    }
    app.stats.signed_in();
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
    if revoked(db, person, device).await? {
        return Err(ApiError::forbidden("device_revoked"));
    }
    Ok(())
}

async fn revoked(db: &mut SqliteConnection, person: &[u8], device: &[u8]) -> ApiResult<bool> {
    Ok(query_scalar(
        "SELECT EXISTS (SELECT 1 FROM revocations WHERE person_id = ? AND device_id = ?)",
    )
    .bind(person)
    .bind(device)
    .fetch_one(db)
    .await?)
}

pub fn client_ip(headers: &HeaderMap) -> ApiResult<String> {
    let values = headers.get_all("x-forwarded-for").iter();
    let ip = rusttalk_core::auth::forwarded_ip(values.map(|value| value.as_bytes()))
        .ok_or(ApiError::bad_request("invalid_forwarded_ip"))?;
    Ok(ip.to_canonical().to_string())
}

/// One IPv6 user owns a whole /64, so rate limits count that prefix as one address.
pub fn limit_key(ip: &str) -> String {
    match ip.parse() {
        Ok(IpAddr::V6(v6)) => {
            std::net::Ipv6Addr::from(u128::from(v6) & !(u128::MAX >> 64)).to_string()
        }
        _ => ip.to_owned(),
    }
}

pub enum PublicLimit {
    InvitePreview,
    OpenJoin,
}

pub async fn check_public_limit(
    db: &mut SqliteConnection,
    ip: &str,
    at: i64,
    limit: PublicLimit,
) -> ApiResult<String> {
    let (table, code, max_total, max_per_ip) = match limit {
        PublicLimit::InvitePreview => (
            "invite_previews",
            "invite_preview_limit",
            PUBLIC_LIMIT,
            PUBLIC_LIMIT_PER_IP,
        ),
        PublicLimit::OpenJoin => ("open_joins", "open_join_limit", 25, 5),
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
    if total >= max_total || from_ip >= max_per_ip {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, code));
    }
    Ok(ip)
}

async fn check_ip(db: &mut SqliteConnection, ip: &str, person: &[u8]) -> ApiResult<()> {
    let banned: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM ban_ips WHERE ip IN (?, ?))
         AND NOT EXISTS (SELECT 1 FROM server_settings WHERE owner_person_id = ?)",
    )
    // IPv6 bans cover the /64; older bans may hold an exact address.
    .bind(ip)
    .bind(limit_key(ip))
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

/// SQL condition: an invite admits people only while its maker may still invite, so kicking,
/// banning or demoting them stops their links. The owner always may. Owner bootstrap invites
/// and invites made before the server recorded creators have no creator and stay valid.
pub const CREATOR_CAN_INVITE: &str = "(invites.created_by IS NULL
    OR invites.created_by = (SELECT owner_person_id FROM server_settings WHERE id = 1)
    OR EXISTS (SELECT 1 FROM member_roles JOIN roles ON roles.id = member_roles.role_id
               WHERE member_roles.person_id = invites.created_by
               AND (roles.permissions & 1) <> 0))";
const _: () = assert!(crate::roles::Permission::Invite as i64 == 1);

pub(crate) async fn redeem(
    db: &mut SqliteConnection,
    person: &[u8],
    invite: &str,
    at: i64,
) -> ApiResult<()> {
    let code = decode::<16>(invite)?;
    // A creator who lost the right gets the same answer as an unknown code.
    let (id, owner, remaining): (String, bool, i64) = query_as(&format!(
        "SELECT id, is_owner, remaining_uses FROM invites
         WHERE code_hash = ? AND expires_at > ? AND {CREATOR_CAN_INVITE}"
    ))
    .bind(hash(&code).as_slice())
    .bind(at)
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::forbidden("invalid_invite"))?;
    add_default_member(db, person, at).await?;
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

async fn admit_open(db: &mut SqliteConnection, person: &[u8], ip: &str, at: i64) -> ApiResult<()> {
    let (open_joining, member_cap): (bool, i64) =
        query_as("SELECT open_joining, member_cap FROM server_settings WHERE id = 1")
            .fetch_one(&mut *db)
            .await?;
    if !open_joining {
        return Err(ApiError::forbidden("open_joining_disabled"));
    }
    let members: i64 = query_scalar("SELECT COUNT(*) FROM members")
        .fetch_one(&mut *db)
        .await?;
    if members >= member_cap {
        return Err(ApiError::new(StatusCode::CONFLICT, "member_cap_reached"));
    }
    let ip = check_public_limit(db, ip, at, PublicLimit::OpenJoin).await?;
    query("INSERT INTO open_joins (ip, expires_at) VALUES (?, ?)")
        .bind(ip)
        .bind(at + 3600)
        .execute(&mut *db)
        .await?;
    add_default_member(db, person, at).await?;
    audit(db, person, "joined", Some(person), None, at).await?;
    Ok(())
}

async fn add_default_member(db: &mut SqliteConnection, person: &[u8], at: i64) -> ApiResult<()> {
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
    Ok(())
}

async fn issue_session(
    db: &mut SqliteConnection,
    person: &[u8],
    device: &[u8],
    ip: &str,
    at: i64,
    ttl: i64,
) -> ApiResult<Session> {
    let token = random::<32>()?;
    let token_hash = hash(&token);
    let expires_at = at + ttl;
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
    query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(token_hash.as_slice()).bind(person).bind(device).bind(ip).bind(expires_at)
        .bind(token_hash.as_slice())
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
    pub session_id: Vec<u8>,
    pub expires_at: i64,
}

pub async fn require_owner(db: &mut SqliteConnection, person: &PersonId) -> ApiResult<()> {
    let owner: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM server_settings WHERE id = 1 AND owner_person_id = ?)",
    )
    .bind(person.as_slice())
    .fetch_one(db)
    .await?;
    if !owner {
        return Err(ApiError::forbidden("owner_required"));
    }
    Ok(())
}

/// Authenticate a request that may write, inside a write transaction. A refused request writes
/// nothing: its transaction rolls back, and the expiry sweep removes expired sessions.
pub async fn authenticate(app: &App, headers: &HeaderMap) -> ApiResult<(WriteTx, Actor)> {
    let (token_hash, ip) = credentials(headers)?;
    let mut tx = match crate::store::try_write(&app.db).await? {
        Some(tx) => tx,
        None => {
            // The writer is busy: refuse unknown or expired sessions from a snapshot first, so
            // they never queue for the writer turn.
            drop(authenticate_read(app, headers).await?);
            write(&app.db).await?
        }
    };
    let actor = actor(&mut tx, &token_hash, ip)
        .await?
        .map_err(|_| ApiError::unauthorized("token_expired"))?;
    app.limits.request(&actor)?;
    Ok((tx, actor))
}

/// Authenticate a read-only request. Its deferred transaction reads a snapshot and never waits
/// for a writer, so it must not write: an upgrade fails at once if another write committed in
/// between. An expired session is refused here and removed by the expiry sweep.
pub async fn authenticate_read(
    app: &App,
    headers: &HeaderMap,
) -> ApiResult<(Transaction<'static, Sqlite>, Actor)> {
    let (token_hash, ip) = credentials(headers)?;
    let mut tx = app.db.begin().await?;
    let actor = actor(&mut tx, &token_hash, ip)
        .await?
        .map_err(|_| ApiError::unauthorized("token_expired"))?;
    app.limits.request(&actor)?;
    Ok((tx, actor))
}

fn credentials(headers: &HeaderMap) -> ApiResult<([u8; 32], String)> {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|value| decode::<32>(value).ok())
        .ok_or(ApiError::unauthorized("invalid_session"))?;
    Ok((hash(&token), client_ip(headers)?))
}

/// The session's actor, or its expiry time if it has expired.
async fn actor(
    db: &mut SqliteConnection,
    token_hash: &[u8; 32],
    ip: String,
) -> ApiResult<Result<Actor, i64>> {
    let at = now()?;
    let (person, device, expires_at, session_id): (Vec<u8>, Vec<u8>, i64, Vec<u8>) = query_as(
        "SELECT person_id, device_id, expires_at, session_id FROM sessions WHERE token_hash = ?",
    )
    .bind(token_hash.as_slice())
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::unauthorized("invalid_session"))?;
    if expires_at <= at {
        return Ok(Err(expires_at));
    }
    check_ip(db, &ip, &person).await?;
    let person = person
        .try_into()
        .map_err(|_| ApiError::internal("invalid_stored_person"))?;
    Ok(Ok(Actor {
        person,
        device,
        ip,
        at,
        session_id,
        expires_at,
    }))
}

pub async fn refresh(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Session>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let token = random::<32>()?;
    let expires_at = actor.at + app.session_ttl;
    query("UPDATE sessions SET token_hash = ?, expires_at = ?, ip = ? WHERE person_id = ? AND device_id = ?")
        .bind(hash(&token).as_slice())
        .bind(expires_at)
        .bind(&actor.ip)
        .bind(actor.person.as_slice())
        .bind(&actor.device)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(reason = "refreshed", "session token rotated");
    app.signal(Change::Session(actor.person));
    Ok(Json(Session {
        token: encode(&token),
        expires_at,
    }))
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
    // Other devices resend the same tombstone during sync, including at the limit.
    if revoked(&mut tx, &actor.person, device).await? {
        return Ok(StatusCode::NO_CONTENT);
    }
    let stored: i64 = query_scalar("SELECT COUNT(*) FROM revocations WHERE person_id = ?")
        .bind(actor.person.as_slice())
        .fetch_one(&mut *tx)
        .await?;
    if stored >= REVOCATIONS_PER_PERSON as i64 {
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
        // Readers skip a revoked device's shared files and page versions, so their files
        // expire like any file.
        crate::files::unshare_from(&app, &mut tx, &actor.person, Some(device)).await?;
        crate::page_files::release_from(&app, &mut tx, &actor.person, Some(device)).await?;
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

    #[test]
    fn one_ip_cannot_take_every_challenge() {
        let mut issued = Challenges(Vec::new());
        let at = 1_000;
        let nonce = |ip: usize, n: usize| {
            let mut nonce = [0; 32];
            nonce[..8].copy_from_slice(&(ip as u32).to_le_bytes().repeat(2));
            nonce[8..16].copy_from_slice(&n.to_le_bytes());
            nonce
        };
        let key = |ip: usize| format!("192.0.2.{ip}");
        // One router past its share keeps working and only loses its own oldest challenge.
        for n in 0..=CHALLENGE_LIMIT_PER_IP {
            issued.issue(nonce(0, n), key(0), at).unwrap();
        }
        assert_eq!(issued.0.len(), CHALLENGE_LIMIT_PER_IP);
        assert_eq!(
            issued.take(&nonce(0, 0), at).unwrap_err().code,
            "invalid_challenge"
        );
        assert!(issued.take(&nonce(0, 1), at).is_ok());
        // Many addresses together hit the server-wide limit until their challenges expire.
        for ip in 1..CHALLENGE_LIMIT / CHALLENGE_LIMIT_PER_IP {
            for n in 0..CHALLENGE_LIMIT_PER_IP {
                issued.issue(nonce(ip, n), key(ip), at).unwrap();
            }
        }
        issued.issue(nonce(0, 999), key(0), at).unwrap();
        let refused = issued.issue(nonce(999, 0), key(999), at + CHALLENGE_TTL - 1);
        assert_eq!(refused.unwrap_err().code, "challenge_limit");
        assert!(
            issued
                .issue(nonce(999, 0), key(999), at + CHALLENGE_TTL)
                .is_ok()
        );
        assert_eq!(issued.0.len(), 1);
        assert_eq!(limit_key("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2::");
        assert_eq!(limit_key("192.0.2.1"), "192.0.2.1");
    }

    #[tokio::test]
    async fn open_join_budget_expires_and_groups_ipv6_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let mut tx = write(&db).await.unwrap();
        query("UPDATE server_settings SET open_joining = TRUE")
            .execute(&mut *tx)
            .await
            .unwrap();
        let at = 100;
        for i in 0..25u8 {
            let ip = format!("2001:db8:{}::{i}", i / 5);
            admit_open(&mut tx, &[i; 32], &ip, at).await.unwrap();
            if i == 4 {
                let refused = admit_open(&mut tx, &[99; 32], "2001:db8:0::ffff", at).await;
                assert_eq!(refused.unwrap_err().code, "open_join_limit");
            }
        }
        let ip = "192.0.2.1";
        let refused = admit_open(&mut tx, &[99; 32], ip, at + 3599).await;
        assert_eq!(refused.unwrap_err().code, "open_join_limit");
        admit_open(&mut tx, &[99; 32], ip, at + 3600).await.unwrap();
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

        query("INSERT INTO ban_ips (person_id, ip) VALUES (x'03', '2001:db8:1:2::')")
            .execute(&mut *conn)
            .await
            .unwrap();
        let rotated = check_ip(&mut conn, "2001:db8:1:2:aaaa::7", &[2]).await;
        assert_eq!(rotated.unwrap_err().code, "ip_banned");
        assert!(check_ip(&mut conn, "2001:db8:1:3::7", &[2]).await.is_ok());
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
            issue_session(
                &mut conn,
                &[1; 32],
                &[at as u8; 32],
                "192.0.2.1",
                at,
                SESSION_TTL,
            )
            .await
            .unwrap();
        }
        issue_session(&mut conn, &[2; 32], &[0; 32], "192.0.2.1", 0, SESSION_TTL)
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

    #[test]
    fn challenges_are_single_use_and_expire() {
        let mut issued = Challenges(Vec::new());
        let at = 1_000;
        issued.issue([1; 32], "192.0.2.1".into(), at).unwrap();
        issued.issue([2; 32], "192.0.2.1".into(), at).unwrap();
        issued.issue([3; 32], "192.0.2.1".into(), at).unwrap();
        assert!(issued.take(&[1; 32], at + CHALLENGE_TTL - 1).is_ok());
        assert_eq!(
            issued.take(&[1; 32], at).unwrap_err().code,
            "invalid_challenge"
        );
        // A late sign-in uses up its expired challenge too.
        assert_eq!(
            issued.take(&[2; 32], at + CHALLENGE_TTL).unwrap_err().code,
            "invalid_challenge"
        );
        assert!(issued.take(&[2; 32], at).is_err());
        assert_eq!(
            issued.take(&[4; 32], at).unwrap_err().code,
            "invalid_challenge"
        );
        assert!(issued.take(&[3; 32], at).is_ok());
    }

    #[tokio::test]
    async fn stored_revocations_block_only_that_person_and_device() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let (words, identity) = DeviceIdentity::create("test").unwrap();
        let person = identity.certificate.person_id();
        let device = identity.certificate.signing_key().as_bytes();
        let revocation = revoke_device(&words, person, *device).unwrap();
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

    /// Person 2's session has expired and person 3 signs in from an IP banned with person 4.
    async fn refusal_app(dir: &std::path::Path) -> App {
        let app = crate::testing::app(dir, 4).await.unwrap();
        for sql in [
            "UPDATE sessions SET expires_at = 1 WHERE device_id = x'02'",
            "INSERT INTO bans (person_id) SELECT person_id FROM sessions WHERE device_id = x'04'",
            "INSERT INTO ban_ips (person_id, ip) SELECT person_id, '192.0.2.9' FROM bans",
        ] {
            query(sql).execute(&app.db).await.unwrap();
        }
        app
    }

    fn refused_requests() -> Vec<(HeaderMap, StatusCode, &'static str)> {
        let mut banned = crate::testing::as_person(3);
        banned.insert("x-forwarded-for", "192.0.2.9".parse().unwrap());
        let mut no_token = crate::testing::as_person(1);
        no_token.remove(AUTHORIZATION);
        vec![
            (no_token, StatusCode::UNAUTHORIZED, "invalid_session"),
            (
                crate::testing::as_person(9),
                StatusCode::UNAUTHORIZED,
                "invalid_session",
            ),
            (
                crate::testing::as_person(2),
                StatusCode::UNAUTHORIZED,
                "token_expired",
            ),
            (banned, StatusCode::FORBIDDEN, "ip_banned"),
        ]
    }

    /// Refused requests leave every session, the expired one included, in place.
    async fn assert_sessions_untouched(app: &App) {
        let rows: Vec<(Vec<u8>, i64)> =
            query_as("SELECT device_id, expires_at FROM sessions ORDER BY device_id")
                .fetch_all(&app.db)
                .await
                .unwrap();
        let expected = (1u8..=4)
            .map(|byte| (vec![byte], if byte == 2 { 1 } else { i64::MAX }))
            .collect::<Vec<_>>();
        assert_eq!(rows, expected);
    }

    #[tokio::test]
    async fn authenticate_without_a_queue_refuses_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let app = refusal_app(dir.path()).await;
        for (headers, status, code) in refused_requests() {
            let refused = authenticate(&app, &headers).await.err().unwrap();
            assert_eq!((refused.status, refused.code), (status, code));
        }
        let (tx, actor) = authenticate(&app, &crate::testing::as_person(3))
            .await
            .unwrap();
        assert_eq!(actor.device, vec![3]);
        drop(tx);
        assert_sessions_untouched(&app).await;
    }

    #[tokio::test]
    async fn authenticate_behind_a_busy_writer_refuses_without_queuing() {
        let dir = tempfile::tempdir().unwrap();
        let app = refusal_app(dir.path()).await;
        let busy = write(&app.db).await.unwrap();
        for (headers, status, code) in refused_requests() {
            let refused =
                tokio::time::timeout(Duration::from_secs(5), authenticate(&app, &headers))
                    .await
                    .expect("refused before the writer turn")
                    .err()
                    .unwrap();
            assert_eq!((refused.status, refused.code), (status, code));
        }
        let signed_in = tokio::spawn({
            let app = app.clone();
            async move {
                let (tx, actor) = authenticate(&app, &crate::testing::as_person(3)).await?;
                drop(tx);
                ApiResult::Ok(actor.device)
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !signed_in.is_finished(),
            "a good token queues for the writer"
        );
        drop(busy);
        assert_eq!(signed_in.await.unwrap().unwrap(), vec![3]);
        assert_sessions_untouched(&app).await;
    }
}
