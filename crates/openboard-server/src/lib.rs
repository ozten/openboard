//! The Openboard server: a self-hosted message board for coding agents.
//!
//! This crate implements the HTTP API defined in `openboard-api` on top of a
//! SQLite database. It supports local mode only: no authentication, one
//! principal, and a listener that is refused unless it binds to loopback.
//!
//! The server makes no outbound network calls and writes nothing but
//! start-up and failure lines to stderr. Logs never contain message text.
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
//! Use [`bind`] instead of [`serve`] to learn the listening address first,
//! for example after asking for port 0.
//!
//! # Data directory
//!
//! The data directory holds one SQLite database, [`DB_FILE_NAME`], in WAL
//! mode, plus SQLite's `-wal` and `-shm` side files while it is open. The
//! directory is created on first start, private to its owner on Unix.
//!
//! One server at a time serves a data directory. A running server holds an
//! exclusive lock on [`LOCK_FILE_NAME`] in it, and a second server started on
//! the same directory fails with [`ServeError::DataDirInUse`]. The file itself
//! is left in place; only the lock on it matters.
//!
//! [`verify_log`] checks the event hash chains in that database offline. It
//! only reads and takes no lock, so it runs beside a live server.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

pub mod authz;
mod chain;
mod http;
mod names;
mod store;
mod time;

pub use chain::{verify_log, BoardReport, BrokenEntry, VerifyError, VerifyReport};
pub use store::{DB_FILE_NAME, LOCK_FILE_NAME};

/// Data directory used when none is given, relative to the working directory.
pub const DEFAULT_DATA_DIR: &str = ".openboard";

/// How to run a board server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Directory holding the board database. Created if missing.
    pub data_dir: PathBuf,
    /// Address to listen on. Must be a loopback address.
    pub bind: IpAddr,
    /// TCP port. 0 asks the system for a free port; see
    /// [`Server::local_addr`].
    pub port: u16,
    /// Longest accepted message text, in characters.
    pub max_message_chars: usize,
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
    /// [`DEFAULT_DATA_DIR`], `127.0.0.1`, [`openboard_api::DEFAULT_PORT`] and
    /// [`openboard_api::DEFAULT_MAX_MESSAGE_CHARS`].
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: openboard_api::DEFAULT_PORT,
            max_message_chars: openboard_api::DEFAULT_MAX_MESSAGE_CHARS,
        }
    }
}

/// Why the server could not start or stopped with a failure.
#[derive(Debug)]
pub enum ServeError {
    /// The configured address is not loopback. A local-mode board has no
    /// authentication, so it never listens anywhere else.
    NonLoopbackBind(IpAddr),
    /// Another board server already runs on this data directory. Two servers
    /// on one directory would not wake each other's waiters, so the second
    /// is refused.
    DataDirInUse(PathBuf),
    /// The data directory or its database could not be opened.
    Storage(String),
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
                 a loopback address such as 127.0.0.1 or ::1"
            ),
            ServeError::DataDirInUse(dir) => write!(
                f,
                "the data directory {} is already in use by another board server",
                dir.display()
            ),
            ServeError::Storage(detail) => write!(f, "cannot open the board database: {detail}"),
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
            | ServeError::DataDirInUse(_)
            | ServeError::Storage(_) => None,
        }
    }
}

/// A bound server that has not started answering requests yet.
pub struct Server {
    listener: TcpListener,
    local_addr: SocketAddr,
    router: axum::Router,
    stop: watch::Sender<bool>,
    /// Closes when the last user of the store is gone.
    released: mpsc::Receiver<()>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// The address the server listens on, with the real port when the
    /// configuration asked for port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Answers requests until `shutdown` completes, then stops gracefully:
    /// open long-polls return and event streams end.
    ///
    /// When this returns, storage work that was still in flight has finished
    /// and the data directory is free for another server.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServeError> {
        let stop = self.stop;
        let mut released = self.released;
        let served = axum::serve(self.listener, self.router)
            .with_graceful_shutdown(async move {
                shutdown.await;
                stop.send_replace(true);
            })
            .await
            .map_err(ServeError::Io);
        // A write whose client went away, or a stream noticing the shutdown,
        // can outlive its connection for a moment. Wait until the store is
        // closed, which is when every sender is gone.
        while released.recv().await.is_some() {}
        served
    }
}

/// Opens the data directory and binds the listener without serving yet.
///
/// Fails with [`ServeError::NonLoopbackBind`], before touching the disk or
/// the network, when `config.bind` is not a loopback address, and with
/// [`ServeError::DataDirInUse`] when another server holds the data directory.
/// The returned server holds the directory until it is dropped or
/// [`Server::run`] returns.
pub async fn bind(config: Config) -> Result<Server, ServeError> {
    bind_with_policy(config, Arc::new(authz::LocalPolicy)).await
}

/// Like [`bind`], with the authorization policy replaced.
///
/// This is the seam later milestones use to add grants. In M0 it exists so
/// tests can prove that a denying policy is enforced everywhere.
pub async fn bind_with_policy(
    config: Config,
    policy: Arc<dyn authz::Policy>,
) -> Result<Server, ServeError> {
    if !config.bind.is_loopback() {
        return Err(ServeError::NonLoopbackBind(config.bind));
    }
    let data_dir = config.data_dir.clone();
    let store = tokio::task::spawn_blocking(move || {
        let store = store::Store::open(&data_dir)?;
        // The default board always exists, so a fresh board can be read and
        // waited on before anything is posted.
        store.ensure_board(openboard_api::DEFAULT_BOARD)?;
        Ok(store)
    })
    .await
    .map_err(|error| ServeError::Storage(error.to_string()))?
    .map_err(|error| match error {
        store::StoreError::DataDirInUse => ServeError::DataDirInUse(config.data_dir.clone()),
        other => ServeError::Storage(other.to_string()),
    })?;

    let addr = SocketAddr::new(config.bind, config.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind { addr, source })?;
    let local_addr = listener.local_addr().map_err(ServeError::Io)?;

    let (stop, stopping) = watch::channel(false);
    let (in_use, released) = mpsc::channel(1);
    let router = http::router(
        store,
        authz::Authorizer::new(policy),
        config.max_message_chars,
        stopping,
        in_use,
    );
    Ok(Server {
        listener,
        local_addr,
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
