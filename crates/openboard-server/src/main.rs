//! `obd`: the Openboard server binary. A thin wrapper over the library.

use std::path::PathBuf;
use std::process::ExitCode;

use openboard_server::{verify_log, Config};

const USAGE: &str = "\
obd: the Openboard server (local mode)

Usage:
  obd [serve] [options]      Run a board on a loopback address
  obd verify-log [options]   Check every board's event hash chain offline

Options:
  --data-dir <dir>             Data directory (default: .openboard)
  --bind <ip>                  Loopback address to listen on (default: 127.0.0.1)
  --port <port>                TCP port (default: 7878)
  --max-message-chars <n>      Longest message, in characters (default: 10000)
  -h, --help                   Show this help
  -V, --version                Show the version

Exit codes: 0 success, 1 failure or a broken log, 2 bad usage.
";

enum Command {
    Serve,
    VerifyLog,
}

fn parse_args(args: Vec<String>) -> Result<Option<(Command, Config)>, String> {
    let mut config = Config::default();
    let mut command = Command::Serve;
    let mut args = args.into_iter().peekable();
    match args.peek().map(String::as_str) {
        Some("serve") => {
            args.next();
        }
        Some("verify-log") => {
            command = Command::VerifyLog;
            args.next();
        }
        _ => {}
    }
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_owned(), Some(value.to_owned()))
            }
            _ => (arg, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("obd {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--data-dir" => config.data_dir = PathBuf::from(value()?),
            "--bind" => {
                config.bind = value()?
                    .parse()
                    .map_err(|_| "--bind needs an IP address such as 127.0.0.1".to_owned())?;
            }
            "--port" => {
                config.port = value()?
                    .parse()
                    .map_err(|_| "--port needs a number from 0 to 65535".to_owned())?;
            }
            "--max-message-chars" => {
                config.max_message_chars = value()?
                    .parse()
                    .map_err(|_| "--max-message-chars needs a whole number".to_owned())?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Some((command, config)))
}

fn main() -> ExitCode {
    let (command, config) = match parse_args(std::env::args().skip(1).collect()) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("obd: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Serve => run_server(config),
        Command::VerifyLog => run_verify(&config),
    }
}

fn run_server(config: Config) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("obd: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let server = openboard_server::bind(config.clone()).await?;
        eprintln!(
            "obd: local board listening on http://{} (data in {})",
            server.local_addr(),
            config.data_dir.display()
        );
        server.run(shutdown_signal()).await
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("obd: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Completes on Ctrl-C or, on Unix, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn run_verify(config: &Config) -> ExitCode {
    let report = match verify_log(&config.data_dir) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("obd: {error}");
            return ExitCode::FAILURE;
        }
    };
    for board in &report.boards {
        match &board.broken {
            None => println!("ok      {}: {} events", board.board, board.verified_events),
            Some(broken) => println!(
                "BROKEN  {}: entry {}: {} ({} earlier events verified)",
                board.board, broken.position, broken.reason, board.verified_events
            ),
        }
    }
    if report.is_ok() {
        println!("event log verified: {} board(s)", report.boards.len());
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
