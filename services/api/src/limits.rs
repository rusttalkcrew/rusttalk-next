//! Per-member rate limits: token buckets in memory, lost on restart. Someone with Manage
//! Server picks a preset in Server settings; the host caps it with `RUSTTALK_RATE_LIMITS`.

use std::{
    collections::HashMap,
    env,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{RateLimitPreset, RateLimits, encode},
    identity::PersonId,
};
use sqlx::{SqliteConnection, query, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate, authenticate_read, limit_key},
    payload,
    roles::{self, Permission},
};

/// What a bucket counts. The server sees only ciphertext, so messages, edits, reactions,
/// poll votes and friend requests are all `Post`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Post,
    /// DM history restored from another server: older copies, posted one by one.
    Backfill,
    Delete,
    Upload,
    UploadBytes,
    NewDm,
    Invite,
    Presence,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rate {
    burst: f64,
    per_sec: f64,
}

const HOUR: f64 = 3600.0;
const MIB: f64 = 1024.0 * 1024.0;

/// The standard rates; the other presets scale them. A person typing, reacting or
/// attaching a full message of photos stays inside these; a script does not.
const fn standard(action: Option<Action>) -> Rate {
    let (burst, per_sec) = match action {
        Some(Action::Post) => (20.0, 1.0),
        Some(Action::Backfill) => (30.0, 2.0),
        Some(Action::Delete) => (10.0, 1.0),
        // A message of ten pictures is twenty uploads with their thumbnails; a folder of
        // photos dropped into shared files goes on at two a second.
        Some(Action::Upload) => (60.0, 2.0),
        Some(Action::UploadBytes) => (512.0 * MIB, 2.0 * MIB),
        Some(Action::NewDm) => (10.0, 10.0 / HOUR),
        Some(Action::Invite) => (20.0, 20.0 / HOUR),
        Some(Action::Presence) => (5.0, 1.0),
        // Every authenticated request of one session. A cold start fetches every avatar
        // and emoji picture at once.
        None => (300.0, 30.0),
    };
    Rate { burst, per_sec }
}

/// Every authenticated request from one address (an IPv6 /64): several people behind
/// one home router.
const PER_IP: f64 = 4.0;

fn rate(preset: RateLimitPreset, action: Option<Action>) -> Rate {
    let scale = match preset {
        RateLimitPreset::Strict => 0.5,
        RateLimitPreset::Standard => 1.0,
        RateLimitPreset::Relaxed => 4.0,
    };
    let Rate { burst, per_sec } = standard(action);
    Rate {
        burst: burst * scale,
        per_sec: per_sec * scale,
    }
}

struct Bucket {
    tokens: f64,
    at: Instant,
    /// When it is full again; a full bucket is the same as none, so it can go.
    full_at: Instant,
}

impl Bucket {
    fn new(rate: Rate, now: Instant) -> Self {
        Self {
            tokens: rate.burst,
            at: now,
            full_at: now,
        }
    }

    fn refill(&mut self, rate: Rate, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate.per_sec).min(rate.burst);
        self.at = now;
    }

    /// Take `cost` tokens, or say how long until they are there.
    fn take(&mut self, cost: f64, rate: Rate, now: Instant) -> Result<(), Duration> {
        self.refill(rate, now);
        if self.tokens < cost {
            return Err(Duration::from_secs_f64((cost - self.tokens) / rate.per_sec));
        }
        self.spend(cost, rate, now);
        Ok(())
    }

    /// Take `cost` after the fact, such as the bytes of a finished upload. The debt is
    /// paid off before the next one may start.
    fn spend(&mut self, cost: f64, rate: Rate, now: Instant) {
        self.refill(rate, now);
        self.tokens = (self.tokens - cost).max(-rate.burst);
        let refill = (rate.burst - self.tokens) / rate.per_sec;
        self.full_at = now + Duration::from_secs_f64(refill);
    }
}

#[derive(PartialEq, Eq, Hash)]
enum Key {
    Person(PersonId, Action),
    Session(Vec<u8>),
    Ip(String),
}

#[derive(Default)]
struct Buckets {
    buckets: HashMap<Key, Bucket>,
    /// When each person was last logged as limited.
    warned: HashMap<PersonId, Instant>,
    swept: Option<Instant>,
}

const SWEEP: Duration = Duration::from_secs(60);

impl Buckets {
    fn take(&mut self, key: Key, cost: f64, rate: Rate, now: Instant) -> Result<(), Duration> {
        if self.swept.is_none_or(|at| now >= at + SWEEP) {
            // Full buckets and old warnings say nothing a fresh one wouldn't.
            self.buckets.retain(|_, bucket| bucket.full_at > now);
            self.warned.retain(|_, at| now < *at + SWEEP);
            self.swept = Some(now);
        }
        let bucket = (self.buckets)
            .entry(key)
            .or_insert_with(|| Bucket::new(rate, now));
        bucket.take(cost, rate, now)
    }

    /// Whether to log this person now: once a minute at most.
    fn warn(&mut self, person: &PersonId, now: Instant) -> bool {
        let last = self.warned.get(person);
        if last.is_some_and(|at| now < *at + SWEEP) {
            return false;
        }
        self.warned.insert(*person, now);
        true
    }
}

pub struct Limits {
    preset: AtomicU8,
    host: Option<RateLimitPreset>,
    buckets: Mutex<Buckets>,
    #[cfg(test)]
    pub(crate) off: bool,
}

const PRESETS: [RateLimitPreset; 3] = [
    RateLimitPreset::Strict,
    RateLimitPreset::Standard,
    RateLimitPreset::Relaxed,
];

impl Limits {
    fn new(host: Option<RateLimitPreset>, saved: Option<RateLimitPreset>) -> Self {
        let limits = Self {
            preset: AtomicU8::new(0),
            host,
            buckets: Mutex::default(),
            #[cfg(test)]
            off: false,
        };
        limits.store(saved);
        limits
    }

    /// The host's cap from the environment and the saved preset.
    pub async fn open(db: &mut SqliteConnection) -> Result<Self> {
        let host = host_from(env::var("RUSTTALK_RATE_LIMITS").ok().as_deref())?;
        Ok(Self::new(host, saved(db).await?))
    }

    #[cfg(test)]
    pub fn for_tests(host: Option<RateLimitPreset>, off: bool) -> Self {
        Self {
            off,
            ..Self::new(host, None)
        }
    }

    /// A saved preset (or the default) within the host's cap.
    fn store(&self, saved: Option<RateLimitPreset>) -> RateLimitPreset {
        let preset = match (saved, self.host) {
            (Some(saved), Some(host)) => saved.min(host),
            (Some(saved), None) => saved,
            (None, host) => host.unwrap_or_default(),
        };
        let index = PRESETS.iter().position(|p| *p == preset).unwrap_or(1);
        self.preset.store(index as u8, Ordering::Relaxed);
        preset
    }

    pub fn preset(&self) -> RateLimitPreset {
        PRESETS[self.preset.load(Ordering::Relaxed) as usize]
    }

    fn take(&self, person: &PersonId, key: Key, cost: f64, rate: Rate) -> ApiResult<()> {
        #[cfg(test)]
        if self.off {
            return Ok(());
        }
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let Err(wait) = buckets.take(key, cost, rate, now) else {
            return Ok(());
        };
        if buckets.warn(person, now) {
            // The person only, never what they sent.
            tracing::warn!(person = encode(person), "member rate limited");
        }
        Err(ApiError::rate_limited(wait))
    }

    /// One `action` by this member.
    pub fn check(&self, actor: &Actor, action: Action) -> ApiResult<()> {
        let rate = rate(self.preset(), Some(action));
        self.take(&actor.person, Key::Person(actor.person, action), 1.0, rate)
    }

    /// May this member start an upload: their byte budget is not in debt.
    pub fn upload(&self, actor: &Actor) -> ApiResult<()> {
        self.check(actor, Action::Upload)?;
        let rate = rate(self.preset(), Some(Action::UploadBytes));
        let key = Key::Person(actor.person, Action::UploadBytes);
        self.take(&actor.person, key, 0.0, rate)
    }

    /// Count a finished upload's bytes.
    pub fn uploaded(&self, actor: &Actor, bytes: i64) {
        #[cfg(test)]
        if self.off {
            return;
        }
        let rate = rate(self.preset(), Some(Action::UploadBytes));
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let key = Key::Person(actor.person, Action::UploadBytes);
        (buckets.buckets.entry(key))
            .or_insert_with(|| Bucket::new(rate, now))
            .spend(bytes as f64, rate, now);
    }

    /// Any authenticated request: the ceiling per session and per address.
    pub fn request(&self, actor: &Actor) -> ApiResult<()> {
        let rate = rate(self.preset(), None);
        let session = Key::Session(actor.session_id.clone());
        self.take(&actor.person, session, 1.0, rate)?;
        let per_ip = Rate {
            burst: rate.burst * PER_IP,
            per_sec: rate.per_sec * PER_IP,
        };
        let ip = Key::Ip(limit_key(&actor.ip));
        self.take(&actor.person, ip, 1.0, per_ip)
    }
}

/// Unset or empty (as Compose passes `${NAME:-}`) is no cap; a set value is both the
/// cap and the default, like the file limits.
fn host_from(value: Option<&str>) -> Result<Option<RateLimitPreset>> {
    match value.filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => serde_json::from_value(value.into())
            .map(Some)
            .context("RUSTTALK_RATE_LIMITS must be strict, standard or relaxed"),
    }
}

async fn saved(db: &mut SqliteConnection) -> Result<Option<RateLimitPreset>, sqlx::Error> {
    let name: Option<String> = query_scalar("SELECT rate_limits FROM server_settings WHERE id = 1")
        .fetch_one(db)
        .await?;
    Ok(name.and_then(|name| serde_json::from_value(name.into()).ok()))
}

fn reply(app: &App) -> Json<RateLimits> {
    Json(RateLimits {
        preset: app.limits.preset(),
        ceiling: app.limits.host,
    })
}

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<RateLimits>> {
    drop(authenticate_read(&app, &headers).await?);
    Ok(reply(&app))
}

pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<RateLimits>, JsonRejection>,
) -> ApiResult<Json<RateLimits>> {
    let requested = payload(body)?.preset;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageServer)?;
    let name = serde_json::to_value(requested)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or(ApiError::internal("invalid_rate_limits"))?;
    query("UPDATE server_settings SET rate_limits = ? WHERE id = 1")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.limits.store(Some(requested));
    Ok(reply(&app))
}

impl ApiError {
    /// 429 with when to try again, in whole seconds.
    pub fn rate_limited(wait: Duration) -> Self {
        Self {
            retry_after: Some(wait.as_secs_f64().ceil().max(1.0) as i64),
            ..Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person};
    use axum::{body::Bytes, http::header::RETRY_AFTER, response::IntoResponse};
    use rusttalk_core::auth::encode;

    const RATE: Rate = Rate {
        burst: 3.0,
        per_sec: 0.5,
    };

    #[test]
    fn a_bucket_spends_its_burst_then_refills_at_its_rate() {
        let start = Instant::now();
        let mut bucket = Bucket::new(RATE, start);
        for _ in 0..3 {
            assert_eq!(bucket.take(1.0, RATE, start), Ok(()));
        }
        assert_eq!(bucket.take(1.0, RATE, start), Err(Duration::from_secs(2)));
        let later = start + Duration::from_secs(1);
        assert_eq!(bucket.take(1.0, RATE, later), Err(Duration::from_secs(1)));
        assert_eq!(
            bucket.take(1.0, RATE, start + Duration::from_secs(2)),
            Ok(())
        );
        // It never fills past its burst, however long it waits.
        let much_later = start + Duration::from_secs(3600);
        for _ in 0..3 {
            assert_eq!(bucket.take(1.0, RATE, much_later), Ok(()));
        }
        assert!(bucket.take(1.0, RATE, much_later).is_err());
    }

    #[test]
    fn a_debt_waits_until_it_is_paid_off() {
        let start = Instant::now();
        let mut bucket = Bucket::new(RATE, start);
        bucket.spend(5.0, RATE, start);
        assert_eq!(bucket.take(0.0, RATE, start), Err(Duration::from_secs(4)));
        assert_eq!(
            bucket.take(0.0, RATE, start + Duration::from_secs(4)),
            Ok(())
        );
        // The debt is bounded, so one huge upload can't lock someone out for long.
        bucket.spend(1e9, RATE, start);
        assert_eq!(bucket.take(0.0, RATE, start), Err(Duration::from_secs(6)));
    }

    #[test]
    fn full_buckets_and_old_warnings_are_swept() {
        let start = Instant::now();
        let mut buckets = Buckets::default();
        let rate = standard(Some(Action::Post));
        for person in 0..100u8 {
            let key = Key::Person([person; 32], Action::Post);
            buckets.take(key, 1.0, rate, start).unwrap();
            assert!(buckets.warn(&[person; 32], start));
            assert!(!buckets.warn(&[person; 32], start));
        }
        assert_eq!(buckets.buckets.len(), 100);
        // One more a while later sweeps the hundred that have refilled meanwhile.
        let later = start + SWEEP;
        buckets
            .take(Key::Ip("192.0.2.1".into()), 1.0, rate, later)
            .unwrap();
        assert_eq!(buckets.buckets.len(), 1);
        assert!(buckets.warned.is_empty());
    }

    #[test]
    fn the_host_caps_the_preset_and_sets_the_default() -> anyhow::Result<()> {
        use RateLimitPreset::*;
        assert_eq!(host_from(None)?, None);
        assert_eq!(host_from(Some(""))?, None);
        assert_eq!(host_from(Some("strict"))?, Some(Strict));
        assert!(host_from(Some("loose")).is_err());

        let free = Limits::new(None, None);
        assert_eq!(free.preset(), Standard);
        assert_eq!(free.store(Some(Relaxed)), Relaxed);
        let capped = Limits::new(Some(Standard), None);
        assert_eq!(capped.preset(), Standard);
        assert_eq!(capped.store(Some(Relaxed)), Standard);
        assert_eq!(capped.store(Some(Strict)), Strict);
        assert_eq!(Limits::new(Some(Strict), Some(Relaxed)).preset(), Strict);
        Ok(())
    }

    /// Post `count` quick messages as person 1 through the message route; the first
    /// refusal's place, status and Retry-After. The others fail later, at the channel
    /// check, which runs after the limit.
    async fn flood(app: &App, count: usize) -> anyhow::Result<Option<(usize, u16, String)>> {
        let channel = encode(&[3; 32]);
        for n in 0..count {
            let body = serde_json::from_value(serde_json::json!({
                "id": encode(&[n as u8; 32]), "epoch": 1, "envelope": "AA", "backfill_at": null,
            }))?;
            let posted = crate::messages::create(
                State(app.clone()),
                as_person(1),
                axum::extract::Path(channel.clone()),
                Ok(Json(body)),
            )
            .await;
            let Err(error) = posted else { continue };
            if error.code != "rate_limited" {
                assert_eq!(error.code, "channel_read_denied");
                continue;
            }
            let response = error.into_response();
            let retry = response.headers()[RETRY_AFTER].to_str()?.to_owned();
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), 1024).await?;
            let expected = format!(r#"{{"error":"rate_limited","retry_after":{retry}}}"#);
            assert_eq!(body, Bytes::from(expected));
            return Ok(Some((n + 1, status, retry)));
        }
        Ok(None)
    }

    fn limited(app: &mut App, host: Option<RateLimitPreset>) {
        app.limits = std::sync::Arc::new(Limits::for_tests(host, false));
    }

    #[tokio::test]
    async fn the_21st_quick_message_is_refused_with_retry_after() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = app(dir.path(), 2).await?;
        limited(&mut app, None);
        assert_eq!(flood(&app, 30).await?, Some((21, 429, "1".into())));
        Ok(())
    }

    #[tokio::test]
    async fn the_owner_preset_changes_the_limit_within_the_host_cap() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = app(dir.path(), 2).await?;
        limited(&mut app, None);
        let strict = || {
            Ok(Json(RateLimits {
                preset: RateLimitPreset::Strict,
                ceiling: None,
            }))
        };
        // Members may read the setting but not change it.
        let Json(read) = get(State(app.clone()), as_person(2)).await?;
        assert_eq!(read.preset, RateLimitPreset::Standard);
        let refused = set(State(app.clone()), as_person(2), strict()).await;
        assert_eq!(refused.unwrap_err().code, "permission_denied");

        let Json(saved) = set(State(app.clone()), as_person(1), strict()).await?;
        assert_eq!(saved.preset, RateLimitPreset::Strict);
        assert_eq!(flood(&app, 30).await?.map(|(n, ..)| n), Some(11));
        // Kept across a restart.
        let mut db = app.db.acquire().await?;
        let reopened = Limits::open(&mut db).await?;
        assert_eq!(reopened.preset(), RateLimitPreset::Strict);

        // A host that caps at Standard keeps an owner's Relaxed at Standard.
        let mut app = app.clone();
        limited(&mut app, Some(RateLimitPreset::Standard));
        let relaxed = RateLimits {
            preset: RateLimitPreset::Relaxed,
            ceiling: None,
        };
        let Json(saved) = set(State(app.clone()), as_person(1), Ok(Json(relaxed))).await?;
        assert_eq!(saved.preset, RateLimitPreset::Standard);
        assert_eq!(saved.ceiling, Some(RateLimitPreset::Standard));
        assert_eq!(flood(&app, 100).await?.map(|(n, ..)| n), Some(21));
        Ok(())
    }

    #[tokio::test]
    async fn every_request_counts_toward_the_session_ceiling() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = app(dir.path(), 2).await?;
        limited(&mut app, Some(RateLimitPreset::Strict));
        let burst = rate(RateLimitPreset::Strict, None).burst as usize;
        // The bucket refills while the loop runs (slow CI machines more so), so allow some
        // slack past the burst, but the ceiling must still hold.
        let mut allowed = 0;
        let refused = loop {
            match authenticate_read(&app, &as_person(2)).await {
                Ok(read) => drop(read),
                Err(refused) => break refused,
            }
            allowed += 1;
            assert!(allowed <= burst * 2, "the session ceiling holds");
        };
        assert!(
            allowed >= burst,
            "the whole burst is allowed: {allowed} < {burst}"
        );
        assert_eq!(refused.code, "rate_limited");
        // Another person's session has its own.
        drop(authenticate_read(&app, &as_person(1)).await?);
        Ok(())
    }
}
