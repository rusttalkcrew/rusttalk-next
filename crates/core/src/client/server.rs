use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
    time::Duration,
};

use crate::{
    auth::{Challenge, Fingerprint, InviteCode, KnownServer, Session, SignIn, check_pin, decode},
    identity::{DeviceIdentity, fingerprint},
    pinned_tls,
};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, RequestBuilder, Response};
use url::Url;

/// A verified RustTalk HTTPS origin and its pinned transport.
pub struct Server {
    http: Client,
    server: KnownServer,
}

impl Server {
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

        let http = pinned_tls::api_client(tls_fingerprint).map_err(anyhow::Error::from_boxed)?;
        let observed = verified_server(&http, address, tls_fingerprint, trusted).await?;
        save_pin(data, &pins, &observed)?;
        Ok(Self {
            http,
            server: observed,
        })
    }

    /// Verify a server proof against `trusted` pins without saving them or sending device credentials.
    pub async fn probe(
        address: Url,
        tls_fingerprint: Option<Fingerprint>,
        trusted: Option<&KnownServer>,
    ) -> Result<Self> {
        validate_origin(&address)?;
        let http = pinned_tls::api_client(tls_fingerprint).map_err(anyhow::Error::from_boxed)?;
        let server = verified_server(&http, address, tls_fingerprint, trusted).await?;
        Ok(Self { http, server })
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
        self.http
            .request(
                method,
                format!("{}{}", self.server.address, path.trim_start_matches('/')),
            )
            .timeout(Duration::from_secs(10))
    }

    /// Verify a fresh server challenge and submit a device proof with an optional invite.
    pub async fn sign_in(
        &self,
        identity: &DeviceIdentity,
        invite: Option<&InviteCode>,
    ) -> Result<Response> {
        let proof = challenge(&self.http, &self.server.address).await?;
        let challenge = proof.verify(&self.server.server_id)?;
        let request = SignIn::signed(identity, &challenge, &self.server.server_id, invite)?;
        self.request(reqwest::Method::POST, "/v1/auth/sign-in")
            .json(&request)
            .send()
            .await
            .map_err(|error| error.without_url().into())
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

fn validate_origin(address: &Url) -> Result<()> {
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
) -> Result<KnownServer> {
    let proof = challenge(http, &address).await?;
    let observed = KnownServer {
        address,
        server_id: decode(&proof.server_id)?,
        tls_fingerprint,
    };
    check_pin(trusted, &observed)?;
    proof.verify(&observed.server_id)?;
    Ok(observed)
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

fn load_pins(file: &Path) -> Result<Vec<KnownServer>> {
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
