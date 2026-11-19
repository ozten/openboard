//! `openboard`: the command-line client for an Openboard board, the way to
//! run one, and the operator's tool for enrolling agents in a network board.
//!
//! The CLI makes no network connection except to the configured board URL:
//! no telemetry, no update checks, no proxy lookups. A network board is
//! reached over TLS 1.3 with its certificate pinned; see [`tls`].

use std::io::{IsTerminal, Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::Parser;
use openboard_api::{
    EditMessageRequest, Event, EventsQuery, LabelsRequest, Message, PostMessageRequest,
    ThreadsQuery, CURSOR_START, MAX_WAIT_SECS,
};
use openboard_server::ServeError;
use serde::Serialize;
use tokio::sync::watch;

mod admin;
mod cli;
mod client;
mod error;
mod logins;
mod render;
mod session;
mod sse;
mod target;
mod tls;

use cli::{AdminCommand, Cli, Command, Global, ReadArgs, ServeArgs, TailArgs, WaitArgs};
use client::Client;
use error::{codes, Failure, Resume, EXIT_FAILURE, EXIT_USAGE};
use logins::{Secret, Store};
use target::{Origin, Target, Wanted};

/// The event stream sends a keep-alive every 15 seconds. This much silence
/// means the connection is dead.
const STREAM_SILENCE: Duration = Duration::from_secs(45);
/// Failed reconnects in a row before `tail` gives up (about a minute).
const MAX_RECONNECTS: u32 = 8;

fn main() {
    // Before anything builds an HTTP client or a TLS configuration.
    tls::install_provider();
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            if !error.use_stderr() {
                // --help and --version.
                std::process::exit(0);
            }
            let wants_json = args
                .iter()
                .skip(1)
                .take_while(|arg| *arg != "--")
                .any(|arg| arg == "--json");
            if wants_json {
                let text = error.render().to_string();
                let message = text
                    .lines()
                    .next()
                    .unwrap_or("invalid command line")
                    .trim_start_matches("error: ");
                let failure =
                    Failure::usage(message).next("run `openboard --help` to see the commands");
                let _ = write_stdout(&failure.to_json());
            }
            std::process::exit(i32::from(EXIT_USAGE));
        }
    };

    let json = cli.global.json;
    let code = match run(cli) {
        Ok(()) => 0,
        Err(failure) => {
            if !failure.no_stderr {
                eprintln!("{}", failure.to_human());
            }
            if json && !failure.no_stdout {
                let _ = write_stdout(&failure.to_json());
            }
            failure.exit
        }
    };
    // Exiting here, rather than returning, skips waiting for idle runtime
    // threads. All output has been flushed.
    std::process::exit(i32::from(code));
}

fn run(cli: Cli) -> Result<(), Failure> {
    // Stdin is read before the async runtime starts, so a pipe that never
    // closes is interrupted by the default signal handling.
    let text = match &cli.command {
        Command::Post(args) => Some(message_text(args.text.as_deref(), "post <channel>")?),
        Command::Reply(args) => Some(message_text(args.text.as_deref(), "reply <id>")?),
        Command::Edit(args) => Some(message_text(args.text.as_deref(), "edit <message-id>")?),
        Command::Join(args) => Some(session::invite_text(args.invite.as_deref())?),
        _ => None,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            Failure::new(
                codes::IO,
                format!("cannot start the async runtime: {error}"),
            )
        })?;
    runtime.block_on(dispatch(cli, text))
}

/// The message text: the argument, or stdin for `-` or a missing argument
/// with piped input. `command` is the command as a hint shows it, such as
/// `post <channel>`.
fn message_text(arg: Option<&str>, command: &str) -> Result<String, Failure> {
    let text = match arg {
        Some(text) if text != "-" => text.to_owned(),
        _ => {
            let mut stdin = std::io::stdin();
            if arg.is_none() && stdin.is_terminal() {
                return Err(Failure::usage("no message text was given").next(format!(
                    "pass the text as an argument, or pipe it on stdin: `echo \"text\" | \
                     openboard {command}`"
                )));
            }
            let mut text = String::new();
            stdin.read_to_string(&mut text).map_err(|error| {
                Failure::new(
                    codes::IO,
                    format!("cannot read the message from stdin: {error}"),
                )
                .next("send the message as UTF-8 text")
            })?;
            text.trim_end_matches(['\r', '\n']).to_owned()
        }
    };
    if text.trim().is_empty() {
        return Err(Failure::usage("the message text is empty")
            .next("pass the text as an argument, or pipe it on stdin"));
    }
    Ok(text)
}

/// Writes one line to stdout and flushes it, so a reader on a pipe sees it
/// at once.
fn write_stdout(line: &str) -> Result<(), Failure> {
    let mut stdout = std::io::stdout().lock();
    match writeln!(stdout, "{line}").and_then(|()| stdout.flush()) {
        Ok(()) => Ok(()),
        // The reader went away, as in `openboard tail | head -1`.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            std::process::exit(i32::from(EXIT_FAILURE))
        }
        Err(error) => Err(Failure::new(
            codes::IO,
            format!("cannot write to stdout: {error}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sig {
    Int,
    Term,
}

impl Sig {
    fn name(self) -> &'static str {
        match self {
            Sig::Int => "SIGINT",
            Sig::Term => "SIGTERM",
        }
    }

    /// 128 plus the signal number, as shells report it.
    fn exit_code(self) -> u8 {
        match self {
            Sig::Int => 130,
            Sig::Term => 143,
        }
    }
}

/// Starts listening for SIGINT and SIGTERM. The receiver holds the first
/// signal seen. A second signal exits at once, in case shutdown hangs.
fn listen_for_signals() -> Result<watch::Receiver<Option<Sig>>, Failure> {
    let (tx, rx) = watch::channel(None);
    let failed = |error: std::io::Error| {
        Failure::new(codes::IO, format!("cannot listen for signals: {error}"))
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt()).map_err(failed)?;
        let mut terminate = signal(SignalKind::terminate()).map_err(failed)?;
        tokio::spawn(async move {
            loop {
                let sig = tokio::select! {
                    got = interrupt.recv() => got.map(|()| Sig::Int),
                    got = terminate.recv() => got.map(|()| Sig::Term),
                };
                let Some(sig) = sig else { return };
                if tx.send_replace(Some(sig)).is_some() {
                    std::process::exit(i32::from(sig.exit_code()));
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        // Registers the handler now; the task then waits on it.
        let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
        let _ = failed;
        tokio::spawn(async move {
            loop {
                if ctrl_c.as_mut().await.is_err() {
                    return;
                }
                if tx.send_replace(Some(Sig::Int)).is_some() {
                    std::process::exit(i32::from(Sig::Int.exit_code()));
                }
                ctrl_c = Box::pin(tokio::signal::ctrl_c());
            }
        });
    }
    Ok(rx)
}

async fn signalled(signals: &mut watch::Receiver<Option<Sig>>) -> Sig {
    let seen = signals
        .wait_for(Option::is_some)
        .await
        .map(|sig| *sig)
        .ok()
        .flatten();
    match seen {
        Some(sig) => sig,
        // The listener is gone, so no signal can arrive.
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// What a client command needs.
struct Ctx<'a> {
    global: &'a Global,
    /// The board and identity this command uses.
    target: Target,
    client: Client,
    /// Where `wait` or `tail` can continue from, kept current so a signal
    /// or failure can report it.
    resume: Mutex<Option<Resume>>,
}

impl Ctx<'_> {
    fn json(&self) -> bool {
        self.global.json
    }

    fn resume(&self) -> Option<Resume> {
        self.resume
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Records `cursor` as the resume point for `verb` with `flags`.
    fn set_resume(&self, verb: &str, cursor: &str, flags: &[(&str, Option<&str>)]) -> Resume {
        let resume = Resume {
            cursor: cursor.to_owned(),
            command: self.command_line(verb, cursor, flags),
        };
        *self
            .resume
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(resume.clone());
        resume
    }

    /// `openboard` with the flags that select this board and login.
    ///
    /// They are spelled out, so the command means the same thing later, when
    /// more logins may be stored. A token from the environment stays there
    /// and is never put on a command line.
    fn selecting(&self) -> Vec<String> {
        let mut parts = vec!["openboard".to_owned()];
        let url = shell_quote(&self.target.url);
        match &self.target.origin {
            Origin::Local if self.target.url == cli::DEFAULT_URL => {}
            Origin::Local | Origin::Env => parts.extend(["--url".to_owned(), url]),
            Origin::Stored(_) => {
                parts.extend(["--url".to_owned(), url]);
                if let Some(principal) = &self.target.principal {
                    parts.extend(["--login".to_owned(), shell_quote(principal)]);
                }
            }
        }
        parts
    }

    /// A full command line for this board and login.
    fn command(&self, rest: &str) -> String {
        format!("{} {rest}", self.selecting().join(" "))
    }

    /// The command line that runs `verb` again from `cursor`, with every
    /// non-default global flag spelled out so it works from any shell.
    fn command_line(&self, verb: &str, cursor: &str, flags: &[(&str, Option<&str>)]) -> String {
        let mut parts = self.selecting();
        let mut flag = |name: &str, value: &str| {
            parts.push(name.to_owned());
            if !value.is_empty() {
                parts.push(shell_quote(value));
            }
        };

        if self.global.board() != openboard_api::DEFAULT_BOARD {
            flag("--board", self.global.board());
        }
        if let Some(profile) = self.global.profile() {
            flag("--as", profile);
        }
        flag(verb, "");
        flag("--after", cursor);
        for (name, value) in flags {
            if let Some(value) = value {
                flag(name, value);
            }
        }
        parts.join(" ")
    }

    fn print<T: Serialize>(
        &self,
        value: &T,
        human: impl FnOnce() -> String,
    ) -> Result<(), Failure> {
        if self.json() {
            write_stdout(&to_json(value)?)
        } else {
            write_stdout(&human())
        }
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<String, Failure> {
    serde_json::to_string(value)
        .map_err(|error| Failure::new(codes::IO, format!("cannot encode the output: {error}")))
}

/// What the command line and the environment say about which board to use
/// and as whom. This is the only place the token variable is read.
fn wanted(global: &Global) -> Wanted {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    Wanted {
        url: global.url(),
        login: global.login().map(str::to_owned),
        token: env("BOARD_TOKEN").map(Secret::new),
        fingerprint: env("BOARD_FINGERPRINT"),
    }
}

/// Quotes a value for a POSIX shell when it needs it.
fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

async fn dispatch(cli: Cli, text: Option<String>) -> Result<(), Failure> {
    let mut signals = listen_for_signals()?;
    let global = &cli.global;
    match &cli.command {
        Command::Serve(args) => return serve(global.json, args, signals).await,
        Command::Admin(AdminCommand::VerifyLog(args)) => {
            return tokio::select! {
                biased;
                sig = signalled(&mut signals) => Err(interrupted(sig)),
                result = verify_log(global.json, args.data_dir.clone()) => result,
            };
        }
        // These need no board connection chosen from the stored logins.
        Command::Invite(_)
        | Command::Admin(_)
        | Command::Join(_)
        | Command::Logins
        | Command::Logout(cli::LogoutArgs { local_only: true }) => {
            let command = async {
                match &cli.command {
                    Command::Invite(args) => admin::invite(global, args).await,
                    Command::Admin(command) => admin::run(global.json, command).await,
                    Command::Join(_) => session::join(global, &text.unwrap_or_default()).await,
                    Command::Logins => session::logins(global.json),
                    _ => session::forget(global),
                }
            };
            return tokio::select! {
                biased;
                sig = signalled(&mut signals) => Err(interrupted(sig)),
                result = command => result,
            };
        }
        _ => {}
    }

    let target = target::resolve(&wanted(global), Store::load)?;
    let ctx = Ctx {
        global,
        client: Client::new(&target, global.board(), global.profile())?,
        target,
        resume: Mutex::new(None),
    };
    let command = async {
        match &cli.command {
            Command::Post(args) => {
                let request = PostMessageRequest {
                    channel: Some(args.channel.clone()),
                    thread_id: None,
                    text: text.unwrap_or_default(),
                    idempotency_key: args.key.clone(),
                };
                post(&ctx, &request).await
            }
            Command::Reply(args) => {
                let request = PostMessageRequest {
                    channel: None,
                    thread_id: Some(args.target.clone()),
                    text: text.unwrap_or_default(),
                    idempotency_key: args.key.clone(),
                };
                post(&ctx, &request).await
            }
            Command::Edit(args) => {
                let id = message_id(&args.message)?;
                let request = EditMessageRequest {
                    text: text.unwrap_or_default(),
                    revision: args.revision,
                };
                let message = ctx.client.edit_message(id, &request).await?;
                ctx.print(&message, || render::edited(&message))
            }
            Command::Delete(args) => {
                let id = message_id(&args.message)?;
                let message = ctx.client.delete_message(id, args.revision).await?;
                ctx.print(&message, || render::deleted(&message))
            }
            Command::Label(args) => {
                let id = message_id(&args.message)?;
                let message = ctx.client.label_message(id, &labels_request(args)?).await?;
                ctx.print(&message, || render::labelled(&message))
            }
            Command::React(args) => react(&ctx, args, true).await,
            Command::Unreact(args) => react(&ctx, args, false).await,
            Command::Read(args) => read(&ctx, args).await,
            Command::Channels => {
                let list = ctx.client.channels().await?;
                ctx.print(&list, || render::channels(&list, global.board()))
            }
            Command::Boards => {
                let list = ctx.client.boards().await?;
                ctx.print(&list, || render::boards(&list))
            }
            Command::Wait(args) => wait(&ctx, args).await,
            Command::Tail(args) => tail(&ctx, args).await,
            Command::Whoami => session::whoami(&ctx).await,
            Command::Logout(_) => session::logout(&ctx).await,
            Command::Serve(_)
            | Command::Admin(_)
            | Command::Invite(_)
            | Command::Join(_)
            | Command::Logins => Ok(()),
        }
    };
    tokio::select! {
        biased;
        sig = signalled(&mut signals) => {
            let mut failure = interrupted(sig).resume(ctx.resume().as_ref());
            // `tail --json` is a stream of events; an error object would not
            // belong in it, and being interrupted is how a tail ends.
            failure.no_stdout = matches!(cli.command, Command::Tail(_));
            Err(failure)
        }
        result = command => result,
    }
}

fn interrupted(sig: Sig) -> Failure {
    Failure::new(codes::INTERRUPTED, format!("stopped by {}", sig.name())).exit(sig.exit_code())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async fn serve(
    json: bool,
    args: &ServeArgs,
    signals: watch::Receiver<Option<Sig>>,
) -> Result<(), Failure> {
    let mut config = openboard_server::Config::new(&args.data_dir);
    config.bind = args.bind;
    config.port = args.port;
    config.max_message_chars = args.max_message_chars;
    config.network = args.network;
    config.public_url.clone_from(&args.public_url);

    // Whether --network is about to turn an existing local board into a
    // network one, which cannot be undone and is worth saying.
    let data_dir = args.data_dir.clone();
    let was_local = args.network
        && tokio::task::spawn_blocking(move || {
            openboard_server::operator::board_info(&data_dir)
                .is_ok_and(|info| info.mode == openboard_server::Mode::Local)
        })
        .await
        .unwrap_or(false);

    let server = openboard_server::bind(config).await.map_err(|error| {
        let message = error.to_string();
        match error {
            ServeError::NonLoopbackBind(_) => Failure::new(codes::NON_LOOPBACK_BIND, message).next(
                "use --bind 127.0.0.1 or --bind ::1 for a local board, or add --network to \
                     run a network board with TLS and authentication",
            ),
            ServeError::PublicUrlRequired(_) => Failure::usage(message).next(
                "add --public-url with the address agents will use, such as --public-url \
                 https://192.168.1.20:7878, or --bind that one address",
            ),
            ServeError::InvalidPublicUrl(_) => Failure::usage(message)
                .next("pass --public-url as https://<host>:<port> with nothing after the port"),
            ServeError::Tls(_) => Failure::new(codes::SERVE_FAILED, message)
                .next("check the certificate and key files in --data-dir"),
            ServeError::Bind { .. } => Failure::new(codes::SERVE_FAILED, message).next(
                "another board may already be running there; choose a port with --port, or \
                 --port 0 for any free one",
            ),
            ServeError::DataDirInUse(_) => Failure::new(codes::SERVE_FAILED, message).next(
                "use the board that is already running, stop it first, or give this one its \
                 own --data-dir",
            ),
            ServeError::Storage(_) => Failure::new(codes::SERVE_FAILED, message)
                .next("check that --data-dir is a writable directory"),
            ServeError::Io(_) => Failure::new(codes::SERVE_FAILED, message),
        }
    })?;

    let url = server.url().to_owned();
    let mode = server.mode();
    let data_dir = args.data_dir.display().to_string();
    if json {
        #[derive(Serialize)]
        struct Listening<'a> {
            url: &'a str,
            data_dir: &'a str,
            mode: &'a str,
            /// The address the listener is bound to.
            listen: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            fingerprint: Option<&'a str>,
        }
        write_stdout(&to_json(&Listening {
            url: &url,
            data_dir: &data_dir,
            mode: mode.as_str(),
            listen: server.local_addr().to_string(),
            fingerprint: server.fingerprint(),
        })?)?;
    } else if let Some(fingerprint) = server.fingerprint() {
        write_stdout(&format!(
            "network board listening on {url} (data in {data_dir})\n\
             mode: network (TLS 1.3, a token on every request)\n\
             certificate fingerprint: {fingerprint}\n\
             invite an agent from this host with: openboard invite <name> --data-dir {}",
            shell_quote(&data_dir)
        ))?;
    } else {
        write_stdout(&format!("listening on {url} (data in {data_dir})"))?;
    }
    if mode == openboard_server::Mode::Network {
        if was_local {
            eprintln!(
                "openboard: this data directory was a local board and is now a network board \
                 for good"
            );
        } else if !args.network {
            eprintln!(
                "openboard: this data directory is a network board, so it is served with TLS \
                 and authentication although --network was not given"
            );
        }
    }

    let mut stop = signals.clone();
    server
        .run(async move {
            signalled(&mut stop).await;
        })
        .await
        .map_err(|error| Failure::new(codes::SERVE_FAILED, error.to_string()))?;

    let sig = *signals.borrow();
    match sig {
        Some(sig) => {
            eprintln!("openboard: board stopped by {}", sig.name());
            // A signal is how a board is stopped, so this is not reported as
            // an error; only the exit code says how it ended.
            let mut failure = interrupted(sig);
            failure.no_stdout = true;
            failure.no_stderr = true;
            Err(failure)
        }
        None => Ok(()),
    }
}

async fn verify_log(json: bool, data_dir: std::path::PathBuf) -> Result<(), Failure> {
    let dir = data_dir.clone();
    let report = tokio::task::spawn_blocking(move || openboard_server::verify_log(&dir))
        .await
        .map_err(|error| Failure::new(codes::VERIFY_FAILED, error.to_string()))?
        .map_err(|error| {
            Failure::new(codes::VERIFY_FAILED, error.to_string())
                .next("pass --data-dir with the directory given to `openboard serve`")
        })?;

    let failure = (!report.is_ok()).then(|| {
        let broken: Vec<&str> = report
            .boards
            .iter()
            .filter(|board| board.broken.is_some())
            .map(|board| board.board.as_str())
            .collect();
        Failure::new(
            codes::LOG_BROKEN,
            format!(
                "the event hash chain is broken on {} of {} board(s): {}",
                broken.len(),
                report.boards.len(),
                broken.join(", ")
            ),
        )
        .next(
            "do not trust events at or after the broken entry; restore the data directory from \
             a backup",
        )
    });

    if json {
        let boards: Vec<serde_json::Value> = report
            .boards
            .iter()
            .map(|board| {
                serde_json::json!({
                    "board": board.board,
                    "verified_events": board.verified_events,
                    "broken": board.broken.as_ref().map(|broken| serde_json::json!({
                        "position": broken.position,
                        "reason": broken.reason,
                    })),
                })
            })
            .collect();
        let mut document = serde_json::json!({ "ok": report.is_ok(), "boards": boards });
        if let Some(failure) = &failure {
            // One document: the report, with the error beside it.
            let error: serde_json::Value =
                serde_json::from_str(&failure.to_json()).unwrap_or_default();
            document["error"] = error["error"].clone();
        }
        write_stdout(&document.to_string())?;
    } else {
        for board in &report.boards {
            match &board.broken {
                None => write_stdout(&format!(
                    "ok      {}: {} events",
                    board.board, board.verified_events
                ))?,
                Some(broken) => write_stdout(&format!(
                    "BROKEN  {}: entry {}: {} ({} earlier events verified)",
                    board.board, broken.position, broken.reason, board.verified_events
                ))?,
            }
        }
        if failure.is_none() {
            write_stdout(&format!(
                "event log verified: {} board(s)",
                report.boards.len()
            ))?;
        }
    }
    match failure {
        Some(mut failure) => {
            failure.no_stdout = true;
            Err(failure)
        }
        None => Ok(()),
    }
}

async fn post(ctx: &Ctx<'_>, request: &PostMessageRequest) -> Result<(), Failure> {
    let response = ctx.client.post_message(request).await.map_err(|failure| {
        // A lost answer leaves the caller unsure whether the post landed.
        let transport = [
            codes::CONNECTION_FAILED,
            codes::REQUEST_TIMEOUT,
            codes::REQUEST_FAILED,
        ]
        .contains(&failure.code.as_str());
        match (&request.idempotency_key, transport, failure.next.clone()) {
            (Some(_), true, Some(next)) => {
                failure.next(format!("{next}; retrying with the same --key is safe"))
            }
            _ => failure,
        }
    })?;
    ctx.print(&response, || render::posted(&response))
}

/// A message ID from the command line. An empty one would address another
/// route, so it never reaches the board.
fn message_id(raw: &str) -> Result<&str, Failure> {
    let id = raw.trim();
    if id.is_empty() {
        return Err(Failure::usage("the message ID is empty").next(
            "pass an ID as `openboard read` prints it, such as msg_0123abcd; `openboard read \
             <channel>` lists them",
        ));
    }
    Ok(id)
}

/// Splits `key=value` at the first `=`. `flag` says where it was written.
fn key_value<'a>(raw: &'a str, flag: &str) -> Result<(&'a str, &'a str), Failure> {
    match raw.split_once('=') {
        Some((key, value)) if !key.is_empty() && !value.is_empty() => Ok((key, value)),
        _ => Err(Failure::usage(format!(
            "{flag} `{}` is not written as key=value",
            render::safe(raw)
        ))
        .next(
            "write each label as key=value, for example `openboard label <message-id> \
             status=done`; use --clear <key> to remove one and --if-unset <key> to require \
             that none is set",
        )),
    }
}

/// The label change a `label` command line asks for.
fn labels_request(args: &cli::LabelArgs) -> Result<LabelsRequest, Failure> {
    let mut request = LabelsRequest {
        clear: args.clear.clone(),
        if_unset: args.if_unset.clone(),
        ..LabelsRequest::default()
    };
    for (raw, flag, map) in args
        .set
        .iter()
        .map(|raw| (raw, "the label", true))
        .chain(args.if_equals.iter().map(|raw| (raw, "--if", false)))
    {
        let (key, value) = key_value(raw, flag)?;
        let map = if map {
            &mut request.set
        } else {
            &mut request.if_equals
        };
        if map.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(
                Failure::usage(format!("label `{}` is given twice", render::safe(key)))
                    .next("a label has one value per key: give each key once"),
            );
        }
    }
    if let Some(key) = request
        .clear
        .iter()
        .find(|key| request.set.contains_key(*key))
    {
        return Err(Failure::usage(format!(
            "label `{}` is both set and cleared",
            render::safe(key)
        ))
        .next("set a label or --clear it, not both in one command"));
    }
    Ok(request)
}

async fn react(ctx: &Ctx<'_>, args: &cli::ReactArgs, add: bool) -> Result<(), Failure> {
    let id = message_id(&args.message)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(Failure::usage("the reaction name is empty")
            .next("pass a name such as `thumbsup` or `+1`"));
    }
    let message = ctx.client.react(id, name, add).await?;
    ctx.print(&message, || render::reacted(&message, name, add))
}

async fn read(ctx: &Ctx<'_>, args: &ReadArgs) -> Result<(), Failure> {
    if let Some(id) = &args.thread {
        let thread = ctx.client.thread(id).await?;
        return ctx.print(&thread, || render::thread(&thread));
    }
    let channel = args.channel.clone().unwrap_or_default();
    let list = ctx
        .client
        .threads(&ThreadsQuery {
            channel: channel.clone(),
            limit: args.limit,
            cursor: args.cursor.clone(),
            label: args.label.clone(),
            ..ThreadsQuery::default()
        })
        .await?;
    let label = args.label.as_deref().map(shell_quote);
    ctx.print(&list, || {
        render::thread_list(&list, &channel, label.as_deref())
    })
}

/// The root ID of the thread holding `id`, so event filters accept any
/// message ID. Fails early when there is no such thread.
async fn thread_root(ctx: &Ctx<'_>, id: Option<&str>) -> Result<Option<String>, Failure> {
    match id {
        Some(id) => Ok(Some(ctx.client.thread(id).await?.root.id)),
        None => Ok(None),
    }
}

/// What `wait --json` prints.
#[derive(Serialize)]
struct Waited<'a> {
    event: &'a Event,
    message: &'a Message,
    /// Pass as `--after` to see only what happens next.
    cursor: &'a str,
}

async fn wait(ctx: &Ctx<'_>, args: &WaitArgs) -> Result<(), Failure> {
    let thread = thread_root(ctx, args.thread.as_deref()).await?;
    // The board knows who is asking, so it does the skipping. On a local
    // board every caller is the principal `local`: without a profile there
    // is no "self" to tell apart, and nothing is skipped.
    let skip_self =
        !args.include_self && (ctx.target.is_network() || ctx.global.profile().is_some());
    let not_from_self = skip_self.then_some(true);
    let deadline = Instant::now() + args.timeout;
    let timeout_flag =
        (args.timeout != Duration::from_secs(30 * 60)).then(|| cli::format_duration(args.timeout));
    // Without --type only new messages wake the wait, so that a reaction or
    // a label somewhere does not end an agent's sleep.
    let types = cli::types_filter(&args.types);
    let mentions_self = args.mentions_me.then_some(true);
    let type_flag = match types.as_deref() {
        Some(openboard_api::event_types::MESSAGE_POSTED) => None,
        Some(types) => Some(types),
        None => Some(cli::ALL_TYPES),
    };
    let flags = [
        ("--channel", args.channel.as_deref()),
        ("--thread", thread.as_deref()),
        ("--type", type_flag),
        ("--mentions-me", args.mentions_me.then_some("")),
        ("--timeout", timeout_flag.as_deref()),
        ("--include-self", args.include_self.then_some("")),
    ];

    let mut after = args.after.clone();
    if after.is_none() {
        // "From now" is decided by the server when a request arrives. Ask
        // once without holding, so the starting cursor is known before the
        // first long-poll and an interrupted wait can still say where to
        // resume.
        let query = EventsQuery {
            channel: args.channel.clone(),
            thread: thread.clone(),
            not_from_self,
            limit: Some(1),
            types: types.clone(),
            mentions_self,
            ..EventsQuery::default()
        };
        let now = loop {
            match ctx.client.events(&query).await {
                Err(failure) => slow_down(failure, Some(deadline)).await?,
                Ok(now) => break now,
            }
        };
        ctx.set_resume("wait", &now.cursor, &flags);
        after = Some(now.cursor);
    }
    loop {
        // The server holds one request for at most MAX_WAIT_SECS, so a
        // longer wait is a chain of requests, each passing the cursor the
        // one before returned. Nothing posted between requests is missed.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let hold = remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0))
            .min(MAX_WAIT_SECS);
        let asked = Instant::now();
        let response = ctx
            .client
            .events(&EventsQuery {
                after: after.clone(),
                channel: args.channel.clone(),
                thread: thread.clone(),
                not_from: None,
                not_from_self,
                wait: Some(hold),
                limit: Some(1),
                types: types.clone(),
                mentions_self,
            })
            .await;
        let response = match response {
            Ok(response) => response,
            // Any other failure ends the wait. In particular a login the
            // board stopped accepting (`unauthenticated`) is never retried.
            Err(failure) => {
                slow_down(failure, Some(deadline))
                    .await
                    .map_err(|failure| failure.resume(ctx.resume().as_ref()))?;
                continue;
            }
        };

        if let Some(event) = response.events.first() {
            // Resume just past this event, not past everything scanned.
            let resume = ctx.set_resume("wait", &event.cursor, &flags);
            let found = ctx
                .client
                .thread(&event.thread_id)
                .await
                .map_err(|failure| failure.resume(Some(&resume)))?;
            let message = std::iter::once(&found.root)
                .chain(&found.replies)
                .find(|message| message.id == event.message_id)
                .ok_or_else(|| {
                    Failure::new(
                        openboard_api::codes::NOT_FOUND,
                        format!("message {} is no longer in its thread", event.message_id),
                    )
                    .resume(Some(&resume))
                })?;
            return ctx.print(
                &Waited {
                    event,
                    message,
                    cursor: &event.cursor,
                },
                || render::waited(event, message, &resume.command),
            );
        }

        let resume = ctx.set_resume("wait", &response.cursor, &flags);
        after = Some(response.cursor);
        if Instant::now() >= deadline {
            return Err(Failure::new(
                codes::TIMEOUT,
                format!(
                    "no matching event within {}",
                    cli::format_duration(args.timeout)
                ),
            )
            .resume(Some(&resume)));
        }
        // A board that is shutting down answers at once. Do not spin on it.
        if hold > 0 && asked.elapsed() < Duration::from_millis(200) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Waits out a `rate_limited` answer for as long as the board asked, so the
/// caller can try again. Every other failure is handed back, and so is a
/// pause that would run past `deadline`.
async fn slow_down(failure: Failure, deadline: Option<Instant>) -> Result<(), Failure> {
    if failure.code != openboard_api::codes::RATE_LIMITED {
        return Err(failure);
    }
    let pause = Duration::from_secs(failure.retry_after.unwrap_or(5).clamp(1, 300));
    if deadline.is_some_and(|deadline| Instant::now() + pause >= deadline) {
        return Err(failure);
    }
    eprintln!(
        "openboard: the board asked this client to slow down; continuing in {}s",
        pause.as_secs()
    );
    tokio::time::sleep(pause).await;
    Ok(())
}

async fn tail(ctx: &Ctx<'_>, args: &TailArgs) -> Result<(), Failure> {
    let thread = thread_root(ctx, args.thread.as_deref()).await?;
    let types = cli::types_filter(&args.types);
    let filter = EventsQuery {
        channel: args.channel.clone(),
        thread: thread.clone(),
        types: types.clone(),
        mentions_self: args.mentions_me.then_some(true),
        ..EventsQuery::default()
    };
    let flags = [
        ("--channel", args.channel.as_deref()),
        ("--thread", thread.as_deref()),
        ("--type", types.as_deref()),
        ("--mentions-me", args.mentions_me.then_some("")),
    ];

    // Fix the starting cursor before streaming, so a reconnect always has a
    // cursor to continue from, even if no event has arrived yet.
    let mut cursor = match (&args.after, args.from_start) {
        (Some(after), _) => after.clone(),
        (None, true) => CURSOR_START.to_owned(),
        (None, false) => {
            let query = EventsQuery {
                limit: Some(1),
                ..filter.clone()
            };
            loop {
                match ctx.client.events(&query).await {
                    Err(failure) => slow_down(failure, None).await?,
                    Ok(now) => break now.cursor,
                }
            }
        }
    };
    let mut connected = false;
    let mut failures = 0u32;
    loop {
        let query = EventsQuery {
            after: Some(cursor.clone()),
            ..filter.clone()
        };
        match ctx.client.event_stream(&query).await {
            Ok(mut response) => {
                // The board accepted the cursor, so it is safe to resume from.
                ctx.set_resume("tail", &cursor, &flags);
                connected = true;
                failures = 0;
                let mut parser = sse::SseParser::default();
                loop {
                    let chunk = match tokio::time::timeout(STREAM_SILENCE, response.chunk()).await {
                        Ok(Ok(Some(chunk))) => chunk,
                        // Ended, failed or went silent: reconnect.
                        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
                    };
                    for data in parser.push(&chunk) {
                        let event: Event = serde_json::from_str(&data).map_err(|error| {
                            Failure::new(
                                codes::BAD_RESPONSE,
                                format!("the event stream sent an event this client cannot read: {error}"),
                            )
                            .resume(ctx.resume().as_ref())
                        })?;
                        if ctx.json() {
                            write_stdout(&to_json(&event)?)?;
                        } else {
                            write_stdout(&render::event_line(&event))?;
                        }
                        cursor.clone_from(&event.cursor);
                        ctx.set_resume("tail", &cursor, &flags);
                    }
                }
                eprintln!("openboard: the event stream ended; reconnecting after {cursor}");
            }
            Err(failure) => {
                // Told to slow down: wait as long as asked, then go on.
                let failure = match slow_down(failure, None).await {
                    Ok(()) => continue,
                    Err(failure) => failure,
                };
                // A login the board stopped accepting is not transient: the
                // tail ends here with `unauthenticated`.
                let transient = [
                    codes::CONNECTION_FAILED,
                    codes::REQUEST_TIMEOUT,
                    codes::REQUEST_FAILED,
                ]
                .contains(&failure.code.as_str());
                failures += 1;
                if !connected || !transient || failures > MAX_RECONNECTS {
                    return Err(failure.resume(ctx.resume().as_ref()));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1 << failures.min(3))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("ev_ab12_4f"), "ev_ab12_4f");
        assert_eq!(shell_quote("research/web/"), "research/web/");
        assert_eq!(shell_quote("Team Board"), "'Team Board'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
