//! End-to-end tests of a board with a supplied certificate (milestone M5):
//! a real network board from the server library, serving a certificate
//! from a certificate authority made for the test, driven by the built
//! `openboard` binary. The binary is told about that authority the way an
//! operator would tell it about a private one, with `SSL_CERT_FILE`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use openboard_api::auth::{fingerprint, Invite};
use openboard_api::ErrorBody;
use openboard_server::operator::{self, InviteOptions};
use openboard_server::{Config, Limits, ServeError, CERT_FILE_NAME};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// A certificate authority for the test
// ---------------------------------------------------------------------------

struct Authority {
    issuer: Issuer<'static, KeyPair>,
    /// The authority's own certificate, which a client is told to trust.
    pem: String,
}

/// A certificate and its key, as PEM.
struct Pair {
    cert: String,
    key: String,
    der: Vec<u8>,
}

impl Authority {
    fn new(name: &str) -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, name);
        let pem = params.self_signed(&key).unwrap().pem();
        Self {
            issuer: Issuer::new(params, key),
            pem,
        }
    }

    /// A certificate for `names`, signed by this authority.
    fn issue(&self, names: &[&str]) -> Pair {
        let key = KeyPair::generate().unwrap();
        let params = CertificateParams::new(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        Pair {
            cert: cert.pem(),
            key: key.serialize_pem(),
            der: cert.der().to_vec(),
        }
    }
}

// ---------------------------------------------------------------------------
// A running board
// ---------------------------------------------------------------------------

struct Running {
    url: String,
    fingerprint: Option<String>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn start(config: Config) -> Result<Self, String> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready, started) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let server = match openboard_server::bind(config).await {
                    Ok(server) => server,
                    Err(error) => {
                        assert!(matches!(error, ServeError::Tls(_)), "{error:?}");
                        ready.send(Err(error.to_string())).unwrap();
                        return;
                    }
                };
                ready
                    .send(Ok((
                        server.url().to_owned(),
                        server.fingerprint().map(str::to_owned),
                    )))
                    .unwrap();
                server
                    .run(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
        });
        match started.recv_timeout(Duration::from_secs(20)).unwrap() {
            Ok((url, fingerprint)) => Ok(Self {
                url,
                fingerprint,
                stop: Some(stop),
                thread: Some(thread),
            }),
            Err(error) => {
                thread.join().unwrap();
                Err(error)
            }
        }
    }

    fn halt(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.halt();
    }
}

/// What a hosted board has on disk: its data, its certificate files and
/// the authority that signed them.
struct Hosted {
    dir: TempDir,
    authority: Authority,
    port: u16,
}

impl Hosted {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let authority = Authority::new("openboard test authority");
        std::fs::write(dir.path().join("authority.pem"), &authority.pem).unwrap();
        let hosted = Self {
            dir,
            authority,
            port: free_port(),
        };
        hosted.install(&hosted.authority.issue(&["localhost"]));
        hosted
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn cert_path(&self) -> PathBuf {
        self.dir.path().join("fullchain.pem")
    }

    fn key_path(&self) -> PathBuf {
        self.dir.path().join("privkey.pem")
    }

    /// The bundle a client trusts in place of the system's authorities.
    fn authority_path(&self) -> PathBuf {
        self.dir.path().join("authority.pem")
    }

    /// The name the certificate is for.
    fn url(&self) -> String {
        format!("https://localhost:{}", self.port)
    }

    fn install(&self, pair: &Pair) {
        std::fs::write(self.cert_path(), &pair.cert).unwrap();
        std::fs::write(self.key_path(), &pair.key).unwrap();
    }

    /// A network board on the data directory, with nothing said about its
    /// certificate.
    fn plain_config(&self) -> Config {
        let mut config = Config::new(self.data_dir());
        config.port = self.port;
        config.network = true;
        config.open = true;
        config.limits = Limits {
            requests_per_minute: 100_000,
            joins_per_minute: 100_000,
            auth_failures_per_minute: 100_000,
            max_waiters: 100,
            credential_recheck: Duration::from_millis(100),
            ..Limits::default()
        };
        config
    }

    fn config(&self) -> Config {
        let mut config = self.plain_config();
        config.public_url = Some(self.url());
        config.tls_cert = Some(self.cert_path());
        config.tls_key = Some(self.key_path());
        config.tls_reload_interval = Duration::from_millis(100);
        config
    }

    fn start(&self) -> Running {
        let running = Running::start(self.config()).unwrap();
        assert_eq!(running.url, self.url());
        assert_eq!(running.fingerprint, None);
        running
    }

    fn invite(&self, name: &str) -> String {
        operator::create_invite(&self.data_dir(), name, &InviteOptions::default())
            .unwrap()
            .invite
            .encode()
    }

    /// A machine that trusts the test authority.
    fn agent(&self) -> Agent {
        Agent {
            home: TempDir::new().unwrap(),
            authorities: Some(self.authority_path()),
        }
    }

    fn enrol(&self, name: &str) -> Agent {
        let agent = self.agent();
        run(agent.cli().args(["join", &self.invite(name)])).ok();
        agent
    }

    /// The certificate the board presents to a new connection right now.
    fn served(&self) -> Vec<u8> {
        #[derive(Debug)]
        struct Anything(Arc<rustls::crypto::CryptoProvider>);
        impl ServerCertVerifier for Anything {
            fn verify_server_cert(
                &self,
                _: &CertificateDer<'_>,
                _: &[CertificateDer<'_>],
                _: &ServerName<'_>,
                _: &[u8],
                _: UnixTime,
            ) -> Result<ServerCertVerified, rustls::Error> {
                Ok(ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _: &[u8],
                _: &CertificateDer<'_>,
                _: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, rustls::Error> {
                Ok(HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _: &[u8],
                _: &CertificateDer<'_>,
                _: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, rustls::Error> {
                Ok(HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                self.0.signature_verification_algorithms.supported_schemes()
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // Only to look at the certificate. Nothing is sent.
        let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Anything(provider)))
            .with_no_client_auth();
        let name = ServerName::try_from("localhost").unwrap();
        let mut conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
        let mut socket = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        while conn.is_handshaking() {
            conn.complete_io(&mut socket).unwrap();
        }
        conn.peer_certificates().unwrap()[0].to_vec()
    }

    /// Waits until the board presents the certificate `der`.
    #[track_caller]
    fn serves(&self, der: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.served() != der {
            assert!(
                Instant::now() < deadline,
                "the certificate was not replaced"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

/// One agent's machine: a configuration directory of its own, and the
/// certificate authorities it trusts.
struct Agent {
    home: TempDir,
    /// `None`: whatever the machine running the tests trusts.
    authorities: Option<PathBuf>,
}

impl Agent {
    fn login_file(&self) -> PathBuf {
        self.home.path().join("config").join("logins.json")
    }

    fn cli(&self) -> Command {
        let mut command = openboard();
        command.env("OPENBOARD_CONFIG_DIR", self.home.path().join("config"));
        if let Some(authorities) = &self.authorities {
            command.env("SSL_CERT_FILE", authorities);
        }
        command
    }

    fn logins(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.login_file()).unwrap()).unwrap()
    }
}

fn openboard() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_openboard"));
    for name in [
        "BOARD_URL",
        "BOARD_NAME",
        "BOARD_AS",
        "BOARD_PROFILE",
        "BOARD_LOGIN",
        "BOARD_TOKEN",
        "BOARD_FINGERPRINT",
        "XDG_CONFIG_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        command.env_remove(name);
    }
    command
        .env(
            "OPENBOARD_CONFIG_DIR",
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-such-config"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[derive(Debug, Clone)]
struct Output {
    code: Option<i32>,
    stdout: String,
    #[allow(dead_code)] // Shown when an assertion fails.
    stderr: String,
}

impl Output {
    #[track_caller]
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {self:?}"))
    }

    #[track_caller]
    fn ok(self) -> Self {
        assert_eq!(self.code, Some(0), "{self:?}");
        self
    }

    #[track_caller]
    fn error(&self, exit: i32, code: &str) -> openboard_api::ApiError {
        assert_eq!(self.code, Some(exit), "{self:?}");
        let body: ErrorBody = serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not an error ({error}): {self:?}"));
        assert_eq!(body.error.code, code, "{self:?}");
        body.error
    }
}

fn run(command: &mut Command) -> Output {
    finish(command.spawn().unwrap(), Duration::from_secs(60))
}

#[track_caller]
fn finish(mut child: Child, limit: Duration) -> Output {
    let deadline = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "the command did not exit within {limit:?}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    Output {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn altered(invite: &str, change: impl FnOnce(&mut Invite)) -> String {
    let mut decoded = Invite::decode(invite).unwrap();
    change(&mut decoded);
    decoded.encode()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn agents_join_a_trusted_board_by_its_host_name_with_no_fingerprint() {
    let hosted = Hosted::new();
    let _board = hosted.start();

    // The operator's side says how the board is trusted and pins nothing.
    let info = run(openboard()
        .args(["--json", "admin", "info", "--data-dir"])
        .arg(hosted.data_dir()))
    .ok()
    .json();
    assert_eq!(info["certificate"], "supplied", "{info}");
    assert_eq!(info["fingerprint"], Value::Null, "{info}");
    assert_eq!(info["url"], hosted.url());
    // The supplied files are served from where they are: the board makes
    // no self-signed certificate beside its data.
    assert!(!hosted.data_dir().join(CERT_FILE_NAME).exists());

    let invited = run(openboard()
        .args(["--json", "invite", "agent-a", "--data-dir"])
        .arg(hosted.data_dir()))
    .ok()
    .json();
    assert_eq!(invited["fingerprint"], Value::Null, "{invited}");
    let invite = invited["invite"].as_str().unwrap().to_owned();
    let decoded = Invite::decode(&invite).unwrap();
    assert_eq!(decoded.fingerprint, None);
    assert_eq!(decoded.url, hosted.url());
    let human = run(openboard()
        .args(["invite", "agent-b", "--data-dir"])
        .arg(hosted.data_dir()))
    .ok();
    assert!(!human.stdout.contains("fingerprint"), "{human:?}");
    assert!(
        human.stdout.contains("certificate authorities"),
        "{human:?}"
    );

    // A person is told what their browser will show.
    let person = run(openboard()
        .args(["invite", "ada@example.com", "--data-dir"])
        .arg(hosted.data_dir()))
    .ok();
    assert!(
        person.stdout.contains("shows no certificate warning"),
        "{person:?}"
    );
    assert!(!person.stdout.contains("self-signed"), "{person:?}");

    // Joining needs the invite and nothing else.
    let agent = hosted.agent();
    let joined = run(agent.cli().args(["--json", "join", &invite]))
        .ok()
        .json();
    assert_eq!(joined["principal"], "agent-a");
    assert!(joined.get("fingerprint").is_none(), "{joined}");
    let login = &agent.logins()["logins"][0];
    assert_eq!(login["url"], hosted.url());
    assert!(login.get("fingerprint").is_none(), "{login}");

    let who = run(agent.cli().args(["--json", "whoami"])).ok().json();
    assert_eq!(who["actor"]["handle"], "agent-a", "{who}");
    run(agent
        .cli()
        .args(["post", "notes", "hello from a host name"]))
    .ok();
    let listed = run(agent.cli().arg("logins")).ok();
    assert!(
        listed.stdout.contains("none (certificate authorities)"),
        "{listed:?}"
    );

    // A token in the environment needs the URL beside it, and no fingerprint.
    let token = login["token"].as_str().unwrap();
    let elsewhere = hosted.agent();
    let who = run(elsewhere
        .cli()
        .env("BOARD_URL", hosted.url())
        .env("BOARD_TOKEN", token)
        .args(["--json", "whoami"]))
    .ok()
    .json();
    assert_eq!(who["actor"]["handle"], "agent-a", "{who}");

    // A derived token is handed on the same way, and clears a fingerprint
    // the worker's shell may hold from another board.
    let minted = run(agent
        .cli()
        .args(["agent", "token", "--can", "notes=read", "--ttl", "10m"]))
    .ok();
    assert!(
        minted.stdout.contains("unset BOARD_FINGERPRINT"),
        "{minted:?}"
    );
    assert!(
        !minted.stdout.contains("export BOARD_FINGERPRINT"),
        "{minted:?}"
    );
}

#[test]
fn a_certificate_no_trusted_authority_vouches_for_is_refused_before_anything_is_sent() {
    let hosted = Hosted::new();
    let _board = hosted.start();
    let invite = hosted.invite("agent-a");

    // A machine that trusts other authorities, and one that trusts
    // whatever the machine running this test does.
    let other = Authority::new("another authority");
    let other_path = hosted.dir.path().join("other.pem");
    std::fs::write(&other_path, &other.pem).unwrap();
    for authorities in [Some(other_path), None] {
        let stranger = Agent {
            home: TempDir::new().unwrap(),
            authorities,
        };
        let out = run(stranger.cli().args(["--json", "join", &invite]));
        let error = out.error(1, "certificate_untrusted");
        assert!(
            error
                .message
                .contains("not signed by a certificate authority"),
            "{out:?}"
        );
        let next = error.next.unwrap();
        assert!(next.contains("nothing was sent"), "{next}");
        assert!(next.contains("BOARD_FINGERPRINT"), "{next}");
        assert!(!stranger.login_file().exists());
    }
    // Nothing was sent: the single-use code is still good.
    let agent = hosted.agent();
    run(agent.cli().args(["join", &invite])).ok();
    let token = agent.logins()["logins"][0]["token"]
        .as_str()
        .unwrap()
        .to_owned();

    // The certificate is for `localhost` and for no other name, the
    // address it is reached at included.
    let by_address = format!("https://127.0.0.1:{}", hosted.port);
    let out = run(hosted
        .agent()
        .cli()
        .env("BOARD_URL", &by_address)
        .env("BOARD_TOKEN", &token)
        .args(["--json", "whoami"]));
    let error = out.error(1, "certificate_untrusted");
    assert!(error.message.contains("is not for 127.0.0.1"), "{out:?}");
    assert!(!format!("{out:?}").contains(&token), "{out:?}");
}

#[test]
fn a_client_never_falls_back_between_a_pin_and_the_certificate_authorities() {
    let hosted = Hosted::new();
    let _board = hosted.start();

    // A pin is the only thing a pinned client accepts, even when the
    // certificate presented is one its certificate authorities vouch for.
    let pinned = altered(&hosted.invite("agent-a"), |invite| {
        invite.fingerprint = Some(fingerprint(&[0x5a; 32]));
    });
    let agent = hosted.agent();
    let out = run(agent.cli().args(["--json", "join", &pinned]));
    out.error(1, "certificate_mismatch");
    assert!(!agent.login_file().exists());

    // And a client that holds no pin never accepts a self-signed
    // certificate, the board's own included.
    let dir = TempDir::new().unwrap();
    let mut config = Config::new(dir.path().join("data"));
    config.port = 0;
    config.network = true;
    let self_signed = Running::start(config).unwrap();
    assert!(self_signed.fingerprint.is_some());
    let invite = operator::create_invite(
        &dir.path().join("data"),
        "agent-a",
        &InviteOptions::default(),
    )
    .unwrap()
    .invite;
    assert_eq!(invite.fingerprint, self_signed.fingerprint);
    let unpinned = altered(&invite.encode(), |invite| invite.fingerprint = None);
    let out = run(agent.cli().args(["--json", "join", &unpinned]));
    out.error(1, "certificate_untrusted");
    assert!(!agent.login_file().exists());
    // The code was never sent, so the invite as issued still works.
    run(agent.cli().args(["join", &invite.encode()])).ok();
}

#[test]
fn a_renewed_certificate_is_served_without_a_restart_and_without_ending_a_wait() {
    let hosted = Hosted::new();
    let _board = hosted.start();
    let first = hosted.served();
    let waiter_agent = hosted.enrol("agent-a");
    let poster = hosted.enrol("agent-b");

    let mut waiter = waiter_agent
        .cli()
        .args(["--json", "wait", "--timeout", "5m"])
        .spawn()
        .unwrap();
    let stderr = waiter.stderr.take().unwrap();
    let (tx, complaints) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    // Let it reach its long-poll.
    std::thread::sleep(Duration::from_millis(1500));

    // A renewal caught half-way, with the new certificate beside the old
    // key, changes nothing.
    let renewed = hosted.authority.issue(&["localhost"]);
    std::fs::write(hosted.cert_path(), &renewed.cert).unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(hosted.served(), first);
    // Nor does a certificate for another name.
    hosted.install(&hosted.authority.issue(&["other.example"]));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(hosted.served(), first);
    // The finished renewal is picked up.
    hosted.install(&renewed);
    hosted.serves(&renewed.der);

    // New connections are verified against the new certificate.
    run(poster.cli().args(["post", "notes", "after the renewal"])).ok();
    // The wait that was open all along is still the same connection, and
    // it gets the event without having reconnected.
    let out = finish(waiter, Duration::from_secs(30)).ok();
    assert!(out.stdout.contains("after the renewal"), "{out:?}");
    let said: Vec<String> = complaints.try_iter().collect();
    assert!(said.is_empty(), "the wait reconnected: {said:?}");
}

#[test]
fn a_board_starts_only_with_a_certificate_that_is_usable_for_its_host() {
    let hosted = Hosted::new();
    let refused = |config: Config| Running::start(config).err().expect("the start is refused");

    // Half a pair.
    let mut config = hosted.config();
    config.tls_key = None;
    assert!(refused(config).contains("go together"));
    // No host name to hold the certificate to.
    let mut config = hosted.config();
    config.public_url = None;
    assert!(refused(config).contains("public URL"));
    // A certificate for another host.
    hosted.install(&hosted.authority.issue(&["other.example"]));
    let error = refused(hosted.config());
    assert!(error.contains("is not for localhost"), "{error}");
    // A key that belongs to another certificate.
    let pair = hosted.authority.issue(&["localhost"]);
    let stray = hosted.authority.issue(&["localhost"]);
    std::fs::write(hosted.cert_path(), &pair.cert).unwrap();
    std::fs::write(hosted.key_path(), &stray.key).unwrap();
    let error = refused(hosted.config());
    assert!(error.contains("does not belong"), "{error}");
    assert!(!error.contains("PRIVATE KEY"), "{error}");
    // None of that made the directory a board.
    assert!(operator::board_info(&hosted.data_dir()).is_err());

    // The intermediates in the file are served after the board's own
    // certificate.
    hosted.install(&pair);
    std::fs::write(
        hosted.cert_path(),
        format!("{}{}", pair.cert, hosted.authority.pem),
    )
    .unwrap();
    let _board = hosted.start();
    assert_eq!(hosted.served(), pair.der);
}

#[test]
fn a_trusted_board_never_goes_back_to_a_self_signed_certificate() {
    let hosted = Hosted::new();
    // It begins as a board with a self-signed certificate, which an agent
    // pins.
    let mut pinned = Running::start(hosted.plain_config()).unwrap();
    let old_fingerprint = pinned.fingerprint.clone().unwrap();
    let agent = hosted.enrol("agent-a");
    assert_eq!(
        agent.logins()["logins"][0]["fingerprint"],
        old_fingerprint.as_str()
    );
    pinned.halt();

    // Then it is given a certificate for its host name.
    let mut board = hosted.start();
    let info = operator::board_info(&hosted.data_dir()).unwrap();
    assert!(info.trusted);
    assert_eq!(info.fingerprint, None);
    assert!(operator::security_log(&hosted.data_dir(), None)
        .unwrap()
        .iter()
        .any(|entry| entry.kind == "certificate.supplied"));
    // The agent that pinned the old certificate does not take the new one
    // on any authority's word. It joins again.
    let out = run(agent.cli().args(["--url", &pinned.url, "--json", "whoami"]));
    out.error(1, "certificate_mismatch");
    let invite = hosted.invite("agent-a");
    assert_eq!(Invite::decode(&invite).unwrap().fingerprint, None);
    let again = hosted.agent();
    run(again.cli().args(["join", &invite])).ok();
    run(again.cli().arg("whoami")).ok();
    board.halt();

    // Started without its certificate, it refuses, and says why.
    let error = Running::start(hosted.plain_config())
        .err()
        .expect("the start is refused");
    assert!(error.contains("certificate authority"), "{error}");
    let info = operator::board_info(&hosted.data_dir()).unwrap();
    assert!(info.trusted);
    assert_eq!(info.fingerprint, None);
    // With it, it starts again and the login still works.
    let _board = hosted.start();
    run(again.cli().arg("whoami")).ok();
}
