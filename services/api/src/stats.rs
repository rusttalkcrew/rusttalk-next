//! Server statistics (#214): counts and sizes only, never content, names or addresses.

pub mod history;

use std::{
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::HeaderMap,
    serve::Listener,
};
use rusttalk_core::auth::{
    ServerStats, StatsDirection, StatsHistory, StatsPeak, StatsStorage, StatsTraffic,
};
use serde::Deserialize;
use sqlx::query_scalar;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
};

use crate::{
    ApiError, ApiResult, App,
    auth::authenticate_read,
    roles::{self, Permission},
    store,
};

/// How often the per-second traffic is worked out, matching how often the window refreshes.
const SAMPLE: Duration = Duration::from_secs(2);

pub struct Stats {
    started: Instant,
    started_at: i64,
    received: AtomicU64,
    sent: AtomicU64,
    logins: AtomicU64,
    messages: AtomicU64,
    /// Most people connected at once, and when.
    peak: Mutex<(u64, i64)>,
    /// Bytes per second over the last sample: received, sent.
    rate: Mutex<(u64, u64)>,
    /// The database's write-ahead log, which `page_count` leaves out.
    wal: Option<PathBuf>,
    pub history: history::History,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            started_at: store::now().unwrap_or_default(),
            received: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            logins: AtomicU64::new(0),
            messages: AtomicU64::new(0),
            peak: Mutex::new((0, 0)),
            rate: Mutex::new((0, 0)),
            wal: None,
            history: Default::default(),
        }
    }
}

impl Stats {
    pub fn new(dir: &Path) -> Self {
        Self {
            wal: Some(dir.join(store::DATABASE).with_extension("sqlite3-wal")),
            ..Self::default()
        }
    }

    /// Bytes received and sent on the API's connections since start.
    pub fn totals(&self) -> (u64, u64) {
        (self.received.load(Relaxed), self.sent.load(Relaxed))
    }

    pub fn signed_in(&self) {
        self.logins.fetch_add(1, Relaxed);
    }

    pub fn message_sent(&self) {
        self.messages.fetch_add(1, Relaxed);
    }

    pub fn connected(&self, now: u64) {
        let mut peak = self.peak.lock().expect("stats peak");
        if now > peak.0 {
            *peak = (now, store::now().unwrap_or_default());
        }
    }
}

/// Work out the per-second traffic every `SAMPLE`.
pub async fn sample(stats: Arc<Stats>) {
    let mut ticks = tokio::time::interval(SAMPLE);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = (tokio::time::Instant::now(), 0, 0);
    loop {
        ticks.tick().await;
        let now = tokio::time::Instant::now();
        let (received, sent) = (stats.received.load(Relaxed), stats.sent.load(Relaxed));
        let millis = now.duration_since(last.0).as_millis().max(1) as u64;
        let per_sec = |total: u64, before: u64| total.saturating_sub(before) * 1000 / millis;
        *stats.rate.lock().expect("stats rate") =
            (per_sec(received, last.1), per_sec(sent, last.2));
        last = (now, received, sent);
    }
}

/// The API listener, counting every byte in and out of its connections.
pub struct Counted {
    pub listener: TcpListener,
    pub stats: Arc<Stats>,
}

impl Listener for Counted {
    type Io = CountedStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, addr) = Listener::accept(&mut self.listener).await;
        let stats = self.stats.clone();
        (CountedStream { stream, stats }, addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

pub struct CountedStream {
    stream: TcpStream,
    stats: Arc<Stats>,
}

impl AsyncRead for CountedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.stream).poll_read(cx, buf);
        let read = buf.filled().len().saturating_sub(before);
        self.stats.received.fetch_add(read as u64, Relaxed);
        polled
    }
}

impl AsyncWrite for CountedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = polled {
            self.stats.sent.fetch_add(written as u64, Relaxed);
        }
        polled
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(written)) = polled {
            self.stats.sent.fetch_add(written as u64, Relaxed);
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub async fn show(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<ServerStats>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ViewServerStats)?;
    let members: i64 = query_scalar("SELECT COUNT(*) FROM members")
        .fetch_one(&mut *tx)
        .await?;
    let files: i64 = query_scalar("SELECT COALESCE(SUM(size), 0) FROM files")
        .fetch_one(&mut *tx)
        .await?;
    let pages: i64 =
        query_scalar("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    let stats = &app.stats;
    let (received_per_sec, sent_per_sec) = *stats.rate.lock().expect("stats rate");
    let (count, at) = *stats.peak.lock().expect("stats peak");
    let (voice, media) = match &app.voice {
        Some(livekit) => livekit.usage().await,
        None => (None, None),
    };
    let wal = match &stats.wal {
        Some(path) => file_size(path).await,
        None => 0,
    };
    Ok(Json(ServerStats {
        version: env!("CARGO_PKG_VERSION").into(),
        started_at: stats.started_at,
        uptime_secs: stats.started.elapsed().as_secs(),
        connected: app.presence.connected() as u64,
        peak: StatsPeak { count, at },
        members,
        logins: stats.logins.load(Relaxed),
        messages: stats.messages.load(Relaxed),
        traffic: StatsTraffic {
            control: StatsDirection {
                received: stats.received.load(Relaxed),
                sent: stats.sent.load(Relaxed),
                received_per_sec,
                sent_per_sec,
            },
            media,
        },
        storage: StatsStorage {
            database: u64::try_from(pages).unwrap_or(0) + wal,
            files: u64::try_from(files).unwrap_or(0),
        },
        memory: memory().await,
        voice,
    }))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    range: i64,
}

/// `GET /v1/stats/history?range=`: one of `history::RANGES`, in seconds.
pub async fn history(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> ApiResult<Json<StatsHistory>> {
    let range = query
        .map_err(|_| ApiError::bad_request("invalid_range"))?
        .0
        .range;
    if !history::RANGES.contains(&range) {
        return Err(ApiError::bad_request("invalid_range"));
    }
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ViewServerStats)?;
    tx.commit().await?;
    Ok(Json(app.stats.history.series(range, store::now()?)))
}

async fn file_size(path: &Path) -> u64 {
    tokio::fs::metadata(path).await.map_or(0, |meta| meta.len())
}

#[cfg(target_os = "linux")]
async fn memory() -> Option<u64> {
    let status = tokio::fs::read_to_string("/proc/self/status").await.ok()?;
    resident(&status)
}

#[cfg(not(target_os = "linux"))]
async fn memory() -> Option<u64> {
    None
}

/// `VmRSS` from `/proc/self/status`, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn resident(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};
    use sqlx::query;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn only_people_allowed_to_view_stats_see_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 4).await?;
        // Person 2 is an admin, person 3 may manage the server but not view stats.
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'admin')")
            .bind(person(2))
            .execute(&app.db)
            .await?;
        query("INSERT INTO roles (id, kind, rank, permissions) VALUES ('host', 'custom', 10, 128)")
            .execute(&app.db)
            .await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'host')")
            .bind(person(3))
            .execute(&app.db)
            .await?;
        let shown = |who: u8| {
            let app = app.clone();
            async move { show(State(app), as_person(who)).await.map(drop) }
        };
        assert!(shown(1).await.is_ok());
        assert!(shown(2).await.is_ok());
        for who in [3, 4] {
            assert_eq!(shown(who).await.unwrap_err().code, "permission_denied");
        }
        // Unknown sessions get nothing either.
        assert_eq!(shown(9).await.unwrap_err().code, "invalid_session");
        Ok(())
    }

    #[tokio::test]
    async fn history_takes_a_known_range_and_the_permission() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let asked = |who: u8, query: &str| {
            let app = app.clone();
            let query = Query::try_from_uri(&format!("/v1/stats/history?{query}").parse().unwrap());
            async move { history(State(app), as_person(who), query).await }
        };
        let series = asked(1, "range=3600").await?.0;
        assert_eq!((series.range, series.step), (3600, 60));
        for bad in ["range=60", "range=-300", "range=lots", ""] {
            assert_eq!(asked(1, bad).await.unwrap_err().code, "invalid_range");
        }
        assert_eq!(
            asked(2, "range=300").await.unwrap_err().code,
            "permission_denied"
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_snapshot_holds_counts_and_sizes_only() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 3).await?;
        app.stats.signed_in();
        app.stats.message_sent();
        app.stats.message_sent();
        app.stats.connected(2);
        app.stats.connected(1);
        let value = serde_json::to_value(show(State(app.clone()), as_person(1)).await?.0)?;
        assert_eq!(value["members"], 3);
        assert_eq!(value["logins"], 1);
        assert_eq!(value["messages"], 2);
        // The peak stays at the most seen at once.
        assert_eq!(value["peak"]["count"], 2);
        assert_eq!(value["connected"], 0);
        assert!(value["storage"]["database"].as_u64().unwrap() > 0);
        assert_eq!(value["storage"]["files"], 0);
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        let fields: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            fields,
            [
                "connected",
                "logins",
                "members",
                "memory",
                "messages",
                "peak",
                "started_at",
                "storage",
                "traffic",
                "uptime_secs",
                "version",
                "voice"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_listener_counts_bytes_both_ways() -> anyhow::Result<()> {
        let stats = Arc::new(Stats::default());
        let mut listener = Counted {
            listener: TcpListener::bind("127.0.0.1:0").await?,
            stats: stats.clone(),
        };
        let addr = Listener::local_addr(&listener)?;
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await?;
            stream.write_all(b"hello").await?;
            let mut reply = [0; 3];
            stream.read_exact(&mut reply).await?;
            anyhow::Ok(reply)
        });
        let (mut stream, _) = Listener::accept(&mut listener).await;
        let mut got = [0; 5];
        stream.read_exact(&mut got).await?;
        stream.write_all(b"hey").await?;
        assert_eq!(&client.await??, b"hey");
        assert_eq!(stats.received.load(Relaxed), 5);
        assert_eq!(stats.sent.load(Relaxed), 3);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn traffic_per_second_comes_from_the_last_sample() {
        let stats = Arc::new(Stats::default());
        let sampler = tokio::spawn(sample(stats.clone()));
        tokio::task::yield_now().await;
        stats.received.fetch_add(4000, Relaxed);
        stats.sent.fetch_add(1000, Relaxed);
        tokio::time::sleep(SAMPLE + Duration::from_millis(10)).await;
        assert_eq!(*stats.rate.lock().unwrap(), (2000, 500));
        sampler.abort();
    }

    #[test]
    fn resident_memory_is_read_in_bytes() {
        let status = "Name:\trusttalk-api\nVmRSS:\t   20480 kB\nThreads:\t9\n";
        assert_eq!(resident(status), Some(20480 * 1024));
        assert_eq!(resident("VmRSS:\tlots kB\n"), None);
        assert_eq!(resident("VmRSS:\t18446744073709551615 kB\n"), None);
        assert_eq!(resident(""), None);
    }
}
