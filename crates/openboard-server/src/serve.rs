//! The accept loop: every connection, in both modes, comes through here.
//!
//! It bounds what a client can hold before it has authenticated. A
//! connection takes a place from [`Connections`] when it is accepted, so the
//! number open is capped in total and per client address; one over a cap is
//! closed at once, and the loop never waits for a place. On a network board
//! the TLS handshake runs beside the loop with a time limit, and only so
//! many of one address's connections may be in it at once. A connection
//! that does not speak TLS 1.3, plain HTTP included, never reaches a
//! handler. After that the head of each request must arrive within
//! [`Limits::header_read_timeout`], which also closes a connection left
//! idle between requests. A response that is being produced, such as a
//! long-poll or an event stream, has no time limit.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request};
use axum::Router;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tower_service::Service;

use crate::http::stopping;
use crate::limits::{ConnectionGuard, Connections, Limits};

/// After shutdown begins, connections get this long to finish what they are
/// sending. Long-polls and streams end by themselves well within it; what
/// is left is a client that has stopped reading, and it is cut off.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The address of the client behind a request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientAddr(pub SocketAddr);

/// Accepts and serves connections until `stop` turns true, then lets the
/// open ones finish. With `tls`, every connection must complete a handshake
/// first.
pub(crate) async fn serve(
    tcp: TcpListener,
    tls: Option<TlsAcceptor>,
    router: Router,
    limits: Limits,
    mut stop: watch::Receiver<bool>,
) {
    let connections = Connections::new(&limits);
    let mut tasks = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            () = stopping(&mut stop) => break,
            // Forgets connections that have ended.
            Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
            accepted = tcp.accept() => accepted,
        };
        let (stream, addr) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                use std::io::ErrorKind;
                let transient = matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                );
                if !transient {
                    // Usually the process is out of file descriptors. Wait
                    // for some to free up rather than spin.
                    eprintln!("openboard-server: cannot accept a connection: {error}");
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                        () = stopping(&mut stop) => break,
                    }
                }
                continue;
            }
        };
        // Over a cap, the connection is dropped, which closes it.
        let Some(place) = connections.admit(addr.ip(), tls.is_some()) else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        tasks.spawn(connection(
            stream,
            addr,
            tls.clone(),
            router.clone(),
            place,
            (limits.handshake_timeout, limits.header_read_timeout),
            stop.clone(),
        ));
    }
    // Stop listening, then wait for the connections.
    drop(tcp);
    let finished = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(SHUTDOWN_GRACE, finished)
        .await
        .is_err()
    {
        tasks.shutdown().await;
    }
}

async fn connection(
    stream: TcpStream,
    addr: SocketAddr,
    tls: Option<TlsAcceptor>,
    router: Router,
    mut place: ConnectionGuard,
    (handshake_timeout, header_read_timeout): (Duration, Duration),
    mut stop: watch::Receiver<bool>,
) {
    match tls {
        None => http(stream, addr, router, header_read_timeout, stop).await,
        Some(acceptor) => {
            let handshake = tokio::select! {
                done = tokio::time::timeout(handshake_timeout, acceptor.accept(stream)) => done,
                () = stopping(&mut stop) => return,
            };
            let Ok(Ok(stream)) = handshake else {
                return;
            };
            place.handshake_done();
            http(stream, addr, router, header_read_timeout, stop).await;
        }
    }
    drop(place);
}

/// Serves HTTP/1.1 on one connection until it closes or, once the server is
/// stopping, until the response in progress is complete.
async fn http<I>(
    io: I,
    addr: SocketAddr,
    router: Router,
    header_read_timeout: Duration,
    mut stop: watch::Receiver<bool>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
        // Handlers learn the client address from here and nowhere else.
        request
            .extensions_mut()
            .insert(ConnectInfo(ClientAddr(addr)));
        let mut router = router.clone();
        async move { router.call(request).await }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    let serving = builder.serve_connection(TokioIo::new(io), service);
    tokio::pin!(serving);
    tokio::select! {
        _ = serving.as_mut() => return,
        () = stopping(&mut stop) => {}
    }
    serving.as_mut().graceful_shutdown();
    let _ = serving.await;
}
