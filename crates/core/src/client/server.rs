use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use crate::{
    auth::{
        Challenge, Fingerprint, InviteCode, KnownServer, ServerInfo, Session, SignIn, check_pin,
        decode,
    },
    identity::{DeviceIdentity, fingerprint},
    pinned_tls,
};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, RequestBuilder, Response};
use url::Url;

type RevokedDevice = (crate::identity::PersonId, [u8; 32]);

/// Reuse connect-time replies only well inside the server's 60 second challenge lifetime.
const SPARE_LIFETIME: Duration = Duration::from_secs(30);

/// When a spare reply was requested. The monotonic clock stops while some systems sleep,
/// so the wall clock must agree that the reply is still fresh.
#[derive(Clone, Copy)]
struct Fetched {
    monotonic: Instant,
    wall: SystemTime,
}

impl Fetched {
    fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    fn fresh(self) -> bool {
        self.monotonic.elapsed() < SPARE_LIFETIME
            && self.wall.elapsed().is_ok_and(|age| age < SPARE_LIFETIME)
    }
}

/// Replies already read while connecting, so the first sign-in after connecting needs no
/// repeat round trips. Each is taken at most once, shared by every clone, and dropped when stale.
#[derive(Default)]
struct Spare {
    challenge: Option<([u8; 32], Fetched)>,
    info: Option<(ServerInfo, Fetched)>,
}

/// A verified RustTalk HTTPS origin and its pinned transport.
#[derive(Clone)]
pub struct Server {
    /// Protocol advertised by this origin, used to choose the outgoing message format.
    pub protocol: u32,
    pub(super) revocations: Option<super::revocations::Revocations>,
    known_revocations: Arc<Mutex<HashSet<RevokedDevice>>>,
    spare: Arc<Mutex<Spare>>,
    http: Client,
    server: KnownServer,
}

impl Server {
    /// A test server reached at `address`, with this ID.
    #[cfg(test)]
    pub(super) fn at(mut self, address: Url, server_id: [u8; 32]) -> Self {
        self.server.address = address;
        self.server.server_id = server_id;
        self
    }

    #[cfg(test)]
    pub(super) fn for_test(protocol: u32, profile: &Path) -> Self {
        Self {
            protocol,
            revocations: Some(super::revocations::Revocations::in_profile(profile)),
            known_revocations: Default::default(),
            spare: Default::default(),
            http: Client::new(),
            server: KnownServer {
                address: "https://localhost/".parse().unwrap(),
                server_id: [1; 32],
                tls_fingerprint: None,
            },
        }
    }

    /// A test server whose requests go to `address`, such as a local fake API.
    #[cfg(test)]
    pub(super) fn at(mut self, address: Url) -> Self {
        self.server.address = address;
        self
    }

    /// Verify the origin against saved and supplied pins, then persist the verified pins.
    pub async fn connect(
        address: Url,
        tls_fingerprint: Option<Fingerprint>,
        expected: Option<&KnownServer>,
        data: &Path,
    ) -> Result<Self> {
        validate_origin(&address)?;
        let pins = data.join("servers.json");
        let known = load_pins(&pins)?
            .into_iter()
            .find(|entry| entry.address == address);
        if let (Some(known), Some(expected)) = (&known, expected) {
            check_pin(Some(known), expected)?;
        }
        let trusted = known.as_ref().or(expected);

        let http = pinned_tls::https_client(tls_fingerprint).map_err(anyhow::Error::from_boxed)?;
        let fetched = Fetched::now();
        // Server info is public, so read it alongside the proof and use it only once verified.
        let (verified, info) = tokio::join!(
            verified_server(&http, address.clone(), tls_fingerprint, trusted),
            server_info(&http, &address)
        );
        let (observed, challenge) = verified?;
        save_pin(data, &pins, &observed)?;
        let mut spare = Spare {
            challenge: Some((challenge, fetched)),
            info: None,
        };
        let protocol = match info {
            Ok(info) => {
                let protocol = info.protocol.unwrap_or(1);
                spare.info = Some((info, fetched));
                protocol
            }
            Err(error) if pinned_tls::is_pin_mismatch(error.as_ref()) => return Err(error),
            Err(_) => {
                tracing::warn!("server protocol unavailable, using protocol 1");
                1
            }
        };
        Ok(Self {
            http,
            server: observed,
            known_revocations: Default::default(),
            spare: Arc::new(Mutex::new(spare)),
            revocations: Some(super::revocations::Revocations::in_profile(data)),
            protocol,
        })
    }

    /// Verify a server proof against `trusted` pins without saving them or sending device credentials.
    pub async fn probe(
        address: Url,
        tls_fingerprint: Option<Fingerprint>,
        trusted: Option<&KnownServer>,
    ) -> Result<Self> {
        validate_origin(&address)?;
        let http = pinned_tls::https_client(tls_fingerprint).map_err(anyhow::Error::from_boxed)?;
        let fetched = Fetched::now();
        let (server, challenge) = verified_server(&http, address, tls_fingerprint, trusted).await?;
        Ok(Self {
            http,
            server,
            revocations: None,
            known_revocations: Default::default(),
            spare: Arc::new(Mutex::new(Spare {
                challenge: Some((challenge, fetched)),
                info: None,
            })),
            protocol: 1,
        })
    }

    /// Return the saved pins for an origin, if this data folder has connected to it before.
    pub fn pinned(data: &Path, address: &Url) -> Result<Option<KnownServer>> {
        Ok(load_pins(&data.join("servers.json"))?
            .into_iter()
            .find(|entry| &entry.address == address))
    }

    /// Return the verified origin, server ID and optional TLS fingerprint.
    pub fn known_server(&self) -> &KnownServer {
        &self.server
    }

    /// Build a request confined to the pinned origin.
    pub fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        use crate::presence::{FEATURES, FEATURES_HEADER};
        self.http
            .request(
                method,
                format!("{}{}", self.server.address, path.trim_start_matches('/')),
            )
            .header(FEATURES_HEADER, FEATURES)
            .timeout(Duration::from_secs(10))
    }

    /// Read public admission and version information through the verified server connection.
    /// A call soon after [`Server::connect`] returns the reply read while connecting.
    pub async fn info(&self) -> Result<ServerInfo> {
        let spare = self.spare.lock().expect("spare replies").info.take();
        if let Some((info, fetched)) = spare
            && fetched.fresh()
        {
            return Ok(info);
        }
        server_info(&self.http, &self.server.address).await
    }

    /// The challenge verified while connecting, at most once and only while still fresh.
    /// Taking it before signing means a failed or retried sign-in never signs it again.
    fn spare_challenge(&self) -> Option<[u8; 32]> {
        let (challenge, fetched) = self.spare.lock().expect("spare replies").challenge.take()?;
        fetched.fresh().then_some(challenge)
    }

    /// Verify a fresh server challenge and submit a device proof with an optional invite.
    pub async fn sign_in(
        &self,
        identity: &DeviceIdentity,
        invite: Option<&InviteCode>,
    ) -> Result<Response> {
        self.submit_proof(identity, invite, "/v1/auth/sign-in")
            .await
    }

    /// Explicitly request admission without an invite.
    pub async fn join_open(&self, identity: &DeviceIdentity) -> Result<Response> {
        self.submit_proof(identity, None, "/v1/auth/join").await
    }

    async fn submit_proof(
        &self,
        identity: &DeviceIdentity,
        invite: Option<&InviteCode>,
        path: &str,
    ) -> Result<Response> {
        let challenge = match self.spare_challenge() {
            Some(challenge) => challenge,
            None => challenge(&self.http, &self.server.address)
                .await?
                .verify(&self.server.server_id)?,
        };
        let request = SignIn::signed(identity, &challenge, &self.server.server_id, invite)?;
        self.request(reqwest::Method::POST, path)
            .json(&request)
            .send()
            .await
            .map_err(|error| error.without_url().into())
    }
    /// Publish an owner's signed record to this member server. Replays are idempotent.
    pub async fn publish_revocation(
        &self,
        session: &Session,
        record: &crate::identity::Revocation,
    ) -> Result<()> {
        let id = (*record.person_id(), *record.device_signing_key());
        if self
            .known_revocations
            .lock()
            .expect("known revocations")
            .contains(&id)
        {
            return Ok(());
        }
        let encoded = crate::auth::encode(&record.to_bytes());
        self.request(reqwest::Method::POST, "/v1/me/revocations")
            .bearer_auth(&session.token)
            .json(&serde_json::json!({"record": encoded}))
            .send()
            .await
            .and_then(Response::error_for_status)
            .map_err(reqwest::Error::without_url)?;
        self.note_revocation(record);
        Ok(())
    }

    pub(super) fn note_revocation(&self, record: &crate::identity::Revocation) {
        self.known_revocations
            .lock()
            .expect("known revocations")
            .insert((*record.person_id(), *record.device_signing_key()));
    }

    pub(super) async fn refresh(&self, session: &Session) -> Result<Session> {
        Ok(self
            .request(reqwest::Method::POST, "/v1/auth/refresh")
            .bearer_auth(&session.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

pub(super) fn validate_origin(address: &Url) -> Result<()> {
    ensure!(
        address.scheme() == "https"
            && address.host().is_some()
            && address.username().is_empty()
            && address.password().is_none()
            && address.path() == "/"
            && address.query().is_none()
            && address.fragment().is_none(),
        "expected an HTTPS server origin"
    );
    Ok(())
}

async fn verified_server(
    http: &Client,
    address: Url,
    tls_fingerprint: Option<Fingerprint>,
    trusted: Option<&KnownServer>,
) -> Result<(KnownServer, [u8; 32])> {
    let proof = challenge(http, &address).await?;
    let observed = KnownServer {
        address,
        server_id: decode(&proof.server_id)?,
        tls_fingerprint,
    };
    check_pin(trusted, &observed)?;
    let challenge = proof.verify(&observed.server_id)?;
    Ok((observed, challenge))
}

async fn server_info(client: &Client, address: &Url) -> Result<ServerInfo> {
    Ok(client
        .get(format!("{address}v1/server-info"))
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn challenge(client: &Client, address: &Url) -> Result<Challenge> {
    let reply = client
        .get(format!("{address}v1/auth/challenge"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .and_then(Response::error_for_status)
        .map_err(reqwest::Error::without_url)
        .context("request server proof")?;
    reply
        .json()
        .await
        .map_err(reqwest::Error::without_url)
        .context("read server proof")
}

pub(super) fn load_pins(file: &Path) -> Result<Vec<KnownServer>> {
    match fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("read saved server pins"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("open saved server pins"),
    }
}

/// Replace `file` in `dir` with JSON so a crash leaves the old or new contents, never a torn file.
pub fn save_json(dir: &Path, file: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut temporary, value)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(file)?;
    Ok(())
}

fn save_pin(data: &Path, pins: &Path, observed: &KnownServer) -> Result<()> {
    fs::create_dir_all(data)?;
    let lock = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(data.join("servers.lock"))?;
    lock.lock()?;
    let mut entries = load_pins(pins)?;
    if let Some(known) = entries
        .iter()
        .find(|entry| entry.address == observed.address)
    {
        check_pin(Some(known), observed)?;
    } else {
        entries.push(observed.clone());
        save_json(data, pins, &entries).context("save server pin")?;
        tracing::info!(fingerprint = %fingerprint(&observed.server_id), "server pinned");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGO: Duration = SPARE_LIFETIME;

    /// A server with nothing listening, so any request it makes fails.
    fn with_spare(spare: Spare) -> Server {
        let mut server = Server::for_test(1, Path::new("unused"));
        server.server.address = "https://127.0.0.1:9/".parse().unwrap();
        *server.spare.lock().unwrap() = spare;
        server
    }

    fn challenge_at(fetched: Fetched) -> Server {
        with_spare(Spare {
            challenge: Some(([7; 32], fetched)),
            info: None,
        })
    }

    fn info() -> ServerInfo {
        ServerInfo {
            dm_backfill: true,
            open_joining: false,
            version: None,
            protocol: Some(3),
        }
    }

    #[test]
    fn a_connect_challenge_is_signed_at_most_once() {
        let server = challenge_at(Fetched::now());
        let clone = server.clone();
        assert_eq!(server.spare_challenge(), Some([7; 32]));
        assert_eq!(clone.spare_challenge(), None);
        assert_eq!(server.spare_challenge(), None);
    }

    #[test]
    fn a_stale_connect_challenge_is_dropped() {
        let server = challenge_at(Fetched {
            monotonic: Instant::now().checked_sub(AGO).expect("monotonic clock"),
            wall: SystemTime::now(),
        });
        assert_eq!(server.spare_challenge(), None);
        assert!(server.spare.lock().unwrap().challenge.is_none());
    }

    #[test]
    fn a_challenge_kept_across_sleep_is_dropped() {
        // The monotonic clock may not advance while the computer sleeps; the wall clock does.
        let server = challenge_at(Fetched {
            monotonic: Instant::now(),
            wall: SystemTime::now() - AGO,
        });
        assert_eq!(server.spare_challenge(), None);
    }

    #[test]
    fn a_challenge_from_the_future_is_dropped() {
        // A wall clock turned back cannot prove the challenge is fresh.
        let server = challenge_at(Fetched {
            monotonic: Instant::now(),
            wall: SystemTime::now() + AGO,
        });
        assert_eq!(server.spare_challenge(), None);
    }

    #[tokio::test]
    async fn a_failed_sign_in_never_signs_the_connect_challenge_again() {
        let (_, identity) = DeviceIdentity::create("test").unwrap();
        let server = challenge_at(Fetched::now());
        assert!(server.sign_in(&identity, None).await.is_err());
        assert!(server.spare.lock().unwrap().challenge.is_none());
        // The retry has to ask the server for a new challenge, which fails here.
        let retry = server.sign_in(&identity, None).await.unwrap_err();
        assert!(format!("{retry:#}").contains("request server proof"));
    }

    #[tokio::test]
    async fn the_first_info_reuses_the_connect_reply() {
        let server = with_spare(Spare {
            challenge: None,
            info: Some((info(), Fetched::now())),
        });
        let info = server.info().await.unwrap();
        assert_eq!((info.protocol, info.dm_backfill), (Some(3), true));
        assert!(server.spare.lock().unwrap().info.is_none());
        assert!(server.info().await.is_err());
    }

    #[tokio::test]
    async fn stale_info_is_read_again() {
        let server = with_spare(Spare {
            challenge: None,
            info: Some((
                info(),
                Fetched {
                    monotonic: Instant::now(),
                    wall: SystemTime::now() - AGO,
                },
            )),
        });
        assert!(server.info().await.is_err());
    }
}
