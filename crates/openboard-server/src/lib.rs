//! The Openboard server: a self-hosted message board for coding agents.
//!
//! This crate implements the HTTP API defined in `openboard-api` on top of a
//! SQLite database. A board runs in one of two [`Mode`]s:
//!
//! - **Local**: plain HTTP, no authentication, one principal, and a listener
//!   that is refused unless it binds to loopback.
//! - **Network**: TLS 1.3 on every listener, loopback included, and a bearer
//!   token or a browser session cookie on every request except the health
//!   check, enrolment, sign-in and the web UI's static files. The
//!   certificate is a self-signed one that clients pin by fingerprint, or
//!   one the operator supplies for the board's host name
//!   ([`Config::tls_cert`]), which clients check against their certificate
//!   authorities. See `openboard_api::auth` and `openboard_api::web` for the
//!   contract.
//!
//! In both modes a write that names an `Origin` other than the address it
//! was sent to is refused, and a write made with a session cookie needs the
//! session's anti-CSRF token as well.
//!
//! In both modes the server also serves the web UI's page and its assets,
//! which are compiled into the binary from this crate's `web/` directory
//! and hold no board content, and it renders message text to HTML through
//! an allowlist when a read asks for it. See `openboard_api::web`.
//!
//! The mode belongs to the data directory. A new directory takes the mode
//! of its first start. A local directory started with [`Config::network`]
//! becomes a network one for good, once that start has its port, and a
//! network directory is served in network mode whatever the configuration
//! asks for, so it can never be reached without TLS and authentication. A
//! directory counts as a network one when its record says so, and also when
//! it holds a certificate or key file or any agent, credential or invite:
//! losing the record does not turn it back into a local board.
//!
//! The server makes no outbound network calls and writes nothing but
//! start-up and failure lines to stderr. Logs never contain message text,
//! tokens, session cookie values, enrolment or sign-in codes, or their
//! hashes.
//!
//! # Running a board
//!
//! ```no_run
//! # async fn demo() -> Result<(), openboard_server::ServeError> {
//! let config = openboard_server::Config::new("./board-data");
//! openboard_server::serve(config, async {
//!     let _ = tokio::signal::ctrl_c().await;
//! })
//! .await
//! # }
//! ```
//!
//! Use [`bind`] instead of [`serve`] to learn the listening address, the
//! board's URL and its certificate fingerprint first, for example after
//! asking for port 0:
//!
//! ```no_run
//! # async fn demo() -> Result<(), openboard_server::ServeError> {
//! let mut config = openboard_server::Config::new("./board-data");
//! config.network = true;
//! config.port = 0;
//! let server = openboard_server::bind(config).await?;
//! println!("{} {:?}", server.url(), server.fingerprint());
//! server.run(std::future::pending()).await
//! # }
//! ```
//!
//! # Enrolling agents and people
//!
//! Agents and people are invited, listed and revoked with the functions in
//! [`operator`], which act on the data directory and have no network API.
//! An agent enrols with an invite and gets a token; a person opens a
//! one-time sign-in link in a browser and gets a session.
//!
//! # Data directory
//!
//! The directory is created on first start. On Unix it and the database
//! files in it are private to their owner: a server that finds them
//! accessible to anyone else, for example because the directory already
//! existed, restricts them when it starts and says so on stderr. The
//! database file must not be a symbolic link; the directory may be one. It
//! holds:
//!
//! - [`DB_FILE_NAME`]: one SQLite database in WAL mode, plus SQLite's `-wal`
//!   and `-shm` side files while it is open. It holds the boards, the
//!   principals, credentials and invites (tokens and codes only as SHA-256
//!   digests), the security log, the mode, and the URL and fingerprint the
//!   last network start recorded. A database written by an older version is
//!   migrated forward when a server opens it. The security log is trimmed
//!   to [`Config::security_log`]. The text of a deleted or edited message
//!   is overwritten in this file, and the `-wal` file is emptied straight
//!   after, so neither keeps a copy.
//! - [`LOCK_FILE_NAME`]: see below.
//! - [`CERT_FILE_NAME`] and [`KEY_FILE_NAME`], on a network board: the
//!   self-signed certificate and its private key in PEM. They are created
//!   on the first network start and reused afterwards, so the fingerprint
//!   clients pin stays the same. The key is readable only by its owner.
//!   Removing the pair makes the next start create a new one, after which
//!   every client must enrol again. A board that serves a supplied
//!   certificate reads it from where the operator keeps it and writes
//!   neither file.
//!
//! One server at a time serves a data directory. A running server holds an
//! exclusive lock on [`LOCK_FILE_NAME`] in it, and a second server started on
//! the same directory fails with [`ServeError::DataDirInUse`]. The file itself
//! is left in place; only the lock on it matters.
//!
//! [`verify_log`] checks the event hash chains in that database offline. It
//! only reads and takes no lock, so it runs beside a live server, as do the
//! [`operator`] functions.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

pub mod authz;
mod chain;
pub mod grants;
mod http;
mod identity;
mod limits;
mod markdown;
mod names;
pub mod operator;
mod pages;
mod serve;
mod store;
mod time;
mod tls;

pub use chain::{verify_log, BoardReport, BrokenEntry, VerifyError, VerifyReport};
pub use limits::{Limits, LogRetention};
pub use store::grants::Access;
pub use store::{DB_FILE_NAME, LOCK_FILE_NAME};
pub use tls::{CERTIFICATE_EXPIRY_WARNING_DAYS, CERT_FILE_NAME, KEY_FILE_NAME};

/// How a board is served. See the crate documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Local,
    Network,
}

impl Mode {
    /// `openboard_api::auth::MODE_LOCAL` or `MODE_NETWORK`.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Local => openboard_api::auth::MODE_LOCAL,
            Mode::Network => openboard_api::auth::MODE_NETWORK,
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            openboard_api::auth::MODE_LOCAL => Some(Mode::Local),
            openboard_api::auth::MODE_NETWORK => Some(Mode::Network),
            _ => None,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Checks a board URL for use in invites and returns it without a trailing
/// slash. It must be `https://` followed by a host, with no path, query or
/// spaces.
pub(crate) fn normalize_public_url(url: &str) -> Result<String, &'static str> {
    let trimmed = url.trim_end_matches('/');
    let Some(authority) = trimmed.strip_prefix("https://") else {
        return Err("it must start with https://");
    };
    let plain = !authority.is_empty()
        && authority
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '/' | '?' | '#' | '@' | '\\'));
    if plain {
        Ok(trimmed.to_owned())
    } else {
        Err("it must be https:// followed by a host and an optional port, and nothing else")
    }
}

/// The host of a URL that [`normalize_public_url`] accepted, without its
/// port and without the brackets of an IPv6 address.
pub(crate) fn host_of(url: &str) -> &str {
    let authority = url.strip_prefix("https://").unwrap_or(url);
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    authority.split(':').next().unwrap_or(authority)
}

/// Data directory used when none is given, relative to the working directory.
pub const DEFAULT_DATA_DIR: &str = ".openboard";

/// How to run a board server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Directory holding the board database. Created if missing.
    pub data_dir: PathBuf,
    /// Address to listen on. Must be a loopback address unless the board
    /// runs in network mode.
    pub bind: IpAddr,
    /// TCP port. 0 asks the system for a free port; see
    /// [`Server::local_addr`].
    pub port: u16,
    /// Longest accepted message text, in characters.
    pub max_message_chars: usize,
    /// Run in network mode: TLS and authentication. Starting a local data
    /// directory with this set upgrades it, one way. A data directory that
    /// is already a network one runs in network mode whether or not this is
    /// set; see [`Server::mode`].
    pub network: bool,
    /// Network mode: the base URL clients reach the board by, recorded in
    /// the data directory for invites. It must start with `https://` and
    /// have no path. When `None` it is `https://<bind>:<port>`, which is
    /// only possible when `bind` is a specific address: with `0.0.0.0` or
    /// `::` this must be given, or the server refuses to start.
    pub public_url: Option<String>,
    /// Rate limits, connection limits and timeouts. The connection limits
    /// and timeouts apply in both modes, the rest in network mode.
    pub limits: Limits,
    /// How long the security log is kept, in both modes.
    pub security_log: LogRetention,
    /// Open the instance: any enrolled agent or person may create boards,
    /// and every board admits all agents and people. A fresh network board
    /// is closed without it (default deny). Setting it on a closed instance
    /// opens it; leaving it unset never closes one.
    pub open: bool,
    /// The handle of an enrolled agent or person to give `all` at `*` on
    /// every board at start, for example after an upgrade, when nobody
    /// holds `grant.manage` yet.
    pub admin: Option<String>,
    /// Network mode: a certificate for the host of [`Config::public_url`],
    /// in PEM, with the board's certificate first and its intermediates
    /// after it, served in place of the self-signed one. It goes with
    /// [`Config::tls_key`] and needs [`Config::public_url`]. Clients of
    /// such a board pin nothing: they check the chain against their
    /// certificate authorities, and the host name.
    ///
    /// The board records that it is served this way, and from then on
    /// refuses to start without a supplied certificate, so that it never
    /// goes back to a self-signed one by accident. Supplying a certificate
    /// to a board whose agents pinned its self-signed one makes them enrol
    /// again.
    pub tls_cert: Option<PathBuf>,
    /// The private key of [`Config::tls_cert`], in PEM.
    pub tls_key: Option<PathBuf>,
    /// How often the supplied certificate and key are read again, so that
    /// a renewed certificate is served without a restart.
    pub tls_reload_interval: std::time::Duration,
}

impl Config {
    /// The defaults, with the given data directory.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            ..Self::default()
        }
    }
}

impl Default for Config {
    /// [`DEFAULT_DATA_DIR`], `127.0.0.1`, [`openboard_api::DEFAULT_PORT`],
    /// [`openboard_api::DEFAULT_MAX_MESSAGE_CHARS`], local mode, the default
    /// [`Limits`] and the default [`LogRetention`].
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: openboard_api::DEFAULT_PORT,
            max_message_chars: openboard_api::DEFAULT_MAX_MESSAGE_CHARS,
            network: false,
            public_url: None,
            limits: Limits::default(),
            security_log: LogRetention::default(),
            open: false,
            admin: None,
            tls_cert: None,
            tls_key: None,
            tls_reload_interval: std::time::Duration::from_secs(10),
        }
    }
}

/// Why the server could not start or stopped with a failure.
#[derive(Debug)]
pub enum ServeError {
    /// The configured address is not loopback. A local-mode board has no
    /// authentication, so it never listens anywhere else.
    NonLoopbackBind(IpAddr),
    /// Network mode on an unspecified address such as `0.0.0.0` needs
    /// [`Config::public_url`], because no URL can be derived from it.
    PublicUrlRequired(IpAddr),
    /// [`Config::public_url`] is not usable. Holds the reason.
    InvalidPublicUrl(&'static str),
    /// The board's certificate or key could not be created or loaded.
    Tls(String),
    /// Another board server already runs on this data directory. Two servers
    /// on one directory would not wake each other's waiters, so the second
    /// is refused.
    DataDirInUse(PathBuf),
    /// The data directory or its database could not be opened.
    Storage(String),
    /// [`Config::admin`] names no enrolled agent or person.
    NoSuchAdmin(String),
    /// The listener could not bind, for example because the port is taken.
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// The listener failed while serving.
    Io(std::io::Error),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServeError::NonLoopbackBind(ip) => write!(
                f,
                "refusing to bind {ip}: a local board has no authentication and only listens on \
                 a loopback address such as 127.0.0.1 or ::1; run a network board to listen \
                 elsewhere"
            ),
            ServeError::PublicUrlRequired(ip) => write!(
                f,
                "a network board listening on {ip} needs its public URL, such as \
                 https://192.168.1.20:7878, because clients cannot connect to {ip}"
            ),
            ServeError::InvalidPublicUrl(reason) => write!(f, "invalid public URL: {reason}"),
            ServeError::Tls(detail) => write!(f, "cannot set up TLS: {detail}"),
            ServeError::DataDirInUse(dir) => write!(
                f,
                "the data directory {} is already in use by another board server",
                dir.display()
            ),
            ServeError::Storage(detail) => write!(f, "cannot open the board database: {detail}"),
            ServeError::NoSuchAdmin(name) => write!(
                f,
                "no agent or person is named {name}, so they cannot be made an admin; invite \
                 them first with `openboard invite {name}`"
            ),
            ServeError::Bind { addr, source } => write!(f, "cannot listen on {addr}: {source}"),
            ServeError::Io(source) => write!(f, "the listener failed: {source}"),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServeError::Bind { source, .. } | ServeError::Io(source) => Some(source),
            ServeError::NonLoopbackBind(_)
            | ServeError::PublicUrlRequired(_)
            | ServeError::InvalidPublicUrl(_)
            | ServeError::Tls(_)
            | ServeError::DataDirInUse(_)
            | ServeError::NoSuchAdmin(_)
            | ServeError::Storage(_) => None,
        }
    }
}

/// A bound server that has not started answering requests yet.
pub struct Server {
    /// A network board on which no principal holds `grant.manage`.
    nobody_manages: bool,
    listener: TcpListener,
    /// Set on a network board, where every connection starts with TLS.
    tls: Option<Arc<rustls::ServerConfig>>,
    /// Set when the certificate was supplied: puts a renewed one in service.
    reloader: Option<tls::Reloader>,
    certificate_expires_at: Option<String>,
    limits: Limits,
    maintenance: http::Maintenance,
    local_addr: SocketAddr,
    mode: Mode,
    url: String,
    fingerprint: Option<String>,
    router: axum::Router,
    stop: watch::Sender<bool>,
    /// Closes when the last user of the store is gone.
    released: mpsc::Receiver<()>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("local_addr", &self.local_addr)
            .field("mode", &self.mode)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// The address the server listens on, with the real port when the
    /// configuration asked for port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The mode the board runs in. This is [`Mode::Network`] for a network
    /// data directory even when the configuration did not ask for it.
    /// True for a network board on which nobody can change grants yet: no
    /// principal holds `grant.manage` on any board. The operator gives it
    /// with `openboard admin grant`, or with [`Config::admin`].
    pub fn nobody_manages(&self) -> bool {
        self.nobody_manages
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The board's base URL. On a network board this is
    /// [`Config::public_url`], or `https://<address>:<port>` with the real
    /// port, and it is what invites carry. On a local board it is
    /// `http://<address>:<port>`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The pin of a self-signed certificate, as
    /// `openboard_api::auth::fingerprint` formats it. `None` on a local
    /// board, and on a board that serves a supplied certificate, which
    /// clients check against their certificate authorities instead.
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }

    /// When the supplied certificate this start loaded stops being valid,
    /// as an RFC 3339 timestamp in UTC. `None` on a board with a
    /// self-signed certificate, which does not expire in practice, and on a
    /// local board.
    ///
    /// The board does not renew anything. While it runs it reads a renewed
    /// certificate from the same files, and it says so on stderr once a day
    /// when the one in service has less than
    /// [`CERTIFICATE_EXPIRY_WARNING_DAYS`] days left, which means the
    /// renewal is not happening.
    pub fn certificate_expires_at(&self) -> Option<&str> {
        self.certificate_expires_at.as_deref()
    }

    /// Answers requests until `shutdown` completes, then stops gracefully:
    /// open long-polls return and event streams end. A client that has
    /// stopped reading its response is cut off after a few seconds.
    ///
    /// When this returns, storage work that was still in flight has finished
    /// and the data directory is free for another server.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServeError> {
        let stop = self.stop;
        let mut released = self.released;
        let stopping = stop.subscribe();
        tokio::spawn(self.maintenance.run(stopping.clone()));
        if let Some(reloader) = self.reloader {
            tokio::spawn(reloader.run(stopping.clone()));
        }
        let serving = serve::serve(
            self.listener,
            self.tls.map(tokio_rustls::TlsAcceptor::from),
            self.router,
            self.limits,
            stopping,
        );
        let shutdown = async move {
            shutdown.await;
            stop.send_replace(true);
        };
        tokio::join!(serving, shutdown);
        // A write whose client went away, or a stream noticing the shutdown,
        // can outlive its connection for a moment. Wait until the store is
        // closed, which is when every sender is gone.
        while released.recv().await.is_some() {}
        Ok(())
    }
}

/// Opens the data directory and binds the listener without serving yet.
/// Connections wait in the system's queue until [`Server::run`].
///
/// The board runs in network mode when [`Config::network`] is set or the
/// data directory is already a network one, and in local mode otherwise;
/// see [`Server::mode`]. In network mode the listener speaks TLS 1.3 only,
/// and the certificate is created on the first start.
///
/// Fails with [`ServeError::NonLoopbackBind`], without creating or changing
/// anything, when the board would run in local mode and `config.bind` is not
/// a loopback address, and with [`ServeError::DataDirInUse`] when another
/// server holds the data directory. The returned server holds the directory
/// until it is dropped or [`Server::run`] returns.
pub async fn bind(config: Config) -> Result<Server, ServeError> {
    bind_inner(config, None).await
}

/// Like [`bind`], with the authorization policy replaced.
///
/// [`bind`] uses [`authz::LocalPolicy`] on a local board and
/// [`authz::NetworkPolicy`] on a network one. This is the seam later
/// milestones use to add grants, and it lets tests prove that a denying
/// policy is enforced everywhere. Authentication is not part of the policy:
/// a network board authenticates every caller before the policy is asked.
pub async fn bind_with_policy(
    config: Config,
    policy: Arc<dyn authz::Policy>,
) -> Result<Server, ServeError> {
    bind_inner(config, Some(policy)).await
}

async fn bind_inner(
    config: Config,
    policy: Option<Arc<dyn authz::Policy>>,
) -> Result<Server, ServeError> {
    // Everything that can refuse the start is checked before the disk is
    // written or a port is opened. The mode is read, never created, here.
    let data_dir = config.data_dir.clone();
    let recorded = tokio::task::spawn_blocking(move || store::peek_mode(&data_dir))
        .await
        .map_err(|error| ServeError::Storage(error.to_string()))?;
    let network = config.network || recorded == Some(Mode::Network);
    if !network && !config.bind.is_loopback() {
        return Err(ServeError::NonLoopbackBind(config.bind));
    }
    let public_url = match &config.public_url {
        Some(url) if network => {
            Some(normalize_public_url(url).map_err(ServeError::InvalidPublicUrl)?)
        }
        None if network && config.bind.is_unspecified() => {
            return Err(ServeError::PublicUrlRequired(config.bind));
        }
        _ => None,
    };
    let supplied = match (&config.tls_cert, &config.tls_key) {
        (None, None) => None,
        (Some(cert_path), Some(key_path)) => {
            // The certificate is for a name, and that name is the host of
            // the public URL: an address the board happens to listen on is
            // not something a certificate authority vouches for.
            let Some(url) = &public_url else {
                return Err(ServeError::Tls(
                    "a supplied certificate needs a network board and its public URL, such as \
                     https://board.example, whose host the certificate is for"
                        .to_owned(),
                ));
            };
            Some(tls::Supplied {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                host: host_of(url).to_owned(),
            })
        }
        _ => {
            return Err(ServeError::Tls(
                "a certificate file and its key file go together: give both or neither".to_owned(),
            ))
        }
    };
    let is_supplied = supplied.is_some();
    // Read now, so that a certificate that cannot be served refuses the
    // start before the data directory is created or changed.
    let reload_interval = config.tls_reload_interval;
    let supplied = match supplied {
        None => None,
        Some(supplied) => Some(
            tokio::task::spawn_blocking(move || tls::load_supplied(supplied, reload_interval))
                .await
                .map_err(|error| ServeError::Tls(error.to_string()))?
                .map_err(ServeError::Tls)?,
        ),
    };

    let (data_dir, bind_ip) = (config.data_dir.clone(), config.bind);
    let (open, admin) = (config.open, config.admin.clone());
    let opened = tokio::task::spawn_blocking(move || {
        let store = store::Store::open(&data_dir).map_err(|error| match error {
            store::StoreError::DataDirInUse => ServeError::DataDirInUse(data_dir.clone()),
            other => ServeError::Storage(other.to_string()),
        })?;
        let storage = |error: store::StoreError| ServeError::Storage(error.to_string());
        // The mode is settled under the data directory lock, and only
        // settled: nothing records it yet. It can only be more strict than
        // what was read above.
        let plan = store.plan_mode(network).map_err(storage)?;
        if plan.mode == Mode::Local && !bind_ip.is_loopback() {
            return Err(ServeError::NonLoopbackBind(bind_ip));
        }
        if plan.trusted && !is_supplied {
            return Err(ServeError::Tls(
                "this board is served with a certificate from a certificate authority, and its \
                 clients pin nothing, so it does not start with a self-signed one: give the \
                 certificate and key files again"
                    .to_owned(),
            ));
        }
        // Before the default board, which starts with the grants of the
        // access mode.
        store
            .settle_access(plan.mode == Mode::Network, open)
            .map_err(storage)?;
        if let Some(handle) = &admin {
            if store.make_admin(handle).map_err(storage)?.is_none() {
                return Err(ServeError::NoSuchAdmin(handle.clone()));
            }
        }
        // The default board always exists, so a fresh board can be read and
        // waited on before anything is posted.
        store
            .ensure_board(openboard_api::DEFAULT_BOARD)
            .map_err(storage)?;
        if let Some(handle) = &admin {
            // Again, for a default board created just now.
            store.make_admin(handle).map_err(storage)?;
        }
        let nobody_manages =
            plan.mode == Mode::Network && !store.anyone_manages().map_err(storage)?;
        Ok((store, plan, nobody_manages))
    })
    .await
    .map_err(|error| ServeError::Storage(error.to_string()))?;
    let (store, plan, nobody_manages) = opened?;
    let mode = plan.mode;

    // The port comes before anything that makes the directory a network
    // one. A start that fails here leaves a local directory local.
    let addr = SocketAddr::new(config.bind, config.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind { addr, source })?;
    let local_addr = listener.local_addr().map_err(ServeError::Io)?;

    let (data_dir, bind_ip, retention) = (
        config.data_dir.clone(),
        config.bind,
        config.security_log.clone(),
    );
    let started = tokio::task::spawn_blocking(move || {
        let storage = |error: store::StoreError| ServeError::Storage(error.to_string());
        let identity = match (mode, supplied) {
            (Mode::Local, _) => None,
            (Mode::Network, Some(supplied)) => Some(supplied),
            (Mode::Network, None) => Some(
                tls::load_or_create(&data_dir, bind_ip, plan.pinned).map_err(ServeError::Tls)?,
            ),
        };
        let url = match &identity {
            None => format!("http://{local_addr}"),
            Some(_) => public_url.unwrap_or_else(|| format!("https://{local_addr}")),
        };
        let endpoint = identity.as_ref().map(|identity| store::Endpoint {
            url: &url,
            fingerprint: identity.fingerprint.as_deref(),
        });
        if plan.pinned && is_supplied {
            eprintln!(
                "openboard-server: this board had a self-signed certificate that its agents \
                 pinned, and now serves a supplied one: every agent must join again with a new \
                 invite"
            );
        }
        store.record_start(&plan, endpoint).map_err(storage)?;
        store.prune_log(&retention).map_err(storage)?;
        Ok((store, identity, url))
    })
    .await
    .map_err(|error| ServeError::Storage(error.to_string()))?;
    let (store, identity, url) = started?;
    let (tls, fingerprint, reloader) = match identity {
        None => (None, None, None),
        Some(identity) => (
            Some(identity.config),
            identity.fingerprint,
            identity.reloader,
        ),
    };
    let certificate_expires_at = reloader.as_ref().and_then(tls::Reloader::expires_at);

    let policy = policy.unwrap_or_else(|| match mode {
        Mode::Local => Arc::new(authz::LocalPolicy),
        Mode::Network => Arc::new(authz::NetworkPolicy),
    });
    let (stop, stopping) = watch::channel(false);
    let (in_use, released) = mpsc::channel(1);
    let (router, maintenance) = http::router(http::Parts {
        store,
        authz: authz::Authorizer::new(policy),
        mode,
        max_message_chars: config.max_message_chars,
        limits: config.limits.clone(),
        retention: config.security_log,
        shutdown: stopping,
        in_use,
        hsts: is_supplied,
    });
    Ok(Server {
        nobody_manages,
        listener,
        tls,
        reloader,
        certificate_expires_at,
        limits: config.limits,
        maintenance,
        local_addr,
        mode,
        url,
        fingerprint,
        router,
        stop,
        released,
    })
}

/// Runs a board server until `shutdown` completes.
///
/// Equivalent to [`bind`] followed by [`Server::run`].
pub async fn serve(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    bind(config).await?.run(shutdown).await
}

#[cfg(test)]
mod tests {
    use super::{host_of, normalize_public_url};

    #[test]
    fn hosts_of_public_urls() {
        for (url, host) in [
            ("https://board.example", "board.example"),
            ("https://board.example:7878", "board.example"),
            ("https://192.168.1.20:7878", "192.168.1.20"),
            ("https://[2001:db8::1]:7878", "2001:db8::1"),
            ("https://[::1]", "::1"),
        ] {
            assert_eq!(host_of(url), host);
        }
    }

    #[test]
    fn public_urls() {
        for (given, expected) in [
            ("https://192.168.1.20:7878", "https://192.168.1.20:7878"),
            ("https://board.example/", "https://board.example"),
            ("https://[2001:db8::1]:7878", "https://[2001:db8::1]:7878"),
        ] {
            assert_eq!(normalize_public_url(given), Ok(expected.to_owned()));
        }
        for bad in [
            "",
            "http://192.168.1.20:7878",
            "https://",
            "https://a/b",
            "https://a?b",
            "https://user@a",
            "https://a b",
            "192.168.1.20:7878",
        ] {
            assert!(normalize_public_url(bad).is_err(), "{bad}");
        }
    }
}
