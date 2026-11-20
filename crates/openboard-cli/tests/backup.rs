//! End-to-end tests of `openboard admin backup` and of restoring a backup
//! as docs/backup-and-restore.md describes it: real boards run by the built
//! `openboard serve`, driven by the built `openboard` binary as a
//! subprocess. Every command gets its own `OPENBOARD_CONFIG_DIR` in a
//! temporary directory, so the real user configuration is never touched.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use openboard_api::ErrorBody;
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// A board run by `openboard serve`
// ---------------------------------------------------------------------------

/// `openboard serve --network` on a data directory, as an operator runs it.
struct Served {
    child: Option<Child>,
    url: String,
    fingerprint: String,
}

impl Served {
    /// Starts a network board on `port` and waits until it listens. `extra`
    /// are more flags for `serve`.
    fn start(data_dir: &Path, port: u16, extra: &[&str]) -> Self {
        let mut child = openboard()
            .args(["serve", "--network", "--port", &port.to_string()])
            .args(extra)
            .arg("--data-dir")
            .arg(data_dir)
            .spawn()
            .unwrap();
        let lines = lines_of(&mut child);
        let next = || {
            lines
                .recv_timeout(Duration::from_secs(20))
                .expect("the board printed nothing")
        };
        let first = next();
        let url = first
            .strip_prefix("network board listening on ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_else(|| panic!("{first}"))
            .to_owned();
        let _mode = next();
        let third = next();
        let fingerprint = third
            .strip_prefix("certificate fingerprint: ")
            .unwrap_or_else(|| panic!("{third}"))
            .to_owned();
        // The rest of stdout is drained by the reader thread.
        Self {
            child: Some(child),
            url,
            fingerprint,
        }
    }

    /// Stops the board with SIGTERM, as a service manager would, and waits
    /// until it has exited.
    fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let status = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the board ignored SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A port on loopback that was free a moment ago.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Stdout lines of a running child, readable with a timeout.
fn lines_of(child: &mut Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            // Keep reading after the receiver is gone, so the board never
            // blocks on a full pipe.
            let _ = tx.send(line);
        }
    });
    rx
}

/// One agent's machine: a configuration directory of its own.
struct Agent {
    home: TempDir,
}

impl Agent {
    /// Enrols `name` on the board in `data_dir` with an invite made by the
    /// operator CLI.
    fn enrol(data_dir: &Path, name: &str) -> Self {
        let agent = Agent {
            home: TempDir::new().unwrap(),
        };
        let invited: Value = admin(data_dir, &["--json", "invite", name]).ok().json();
        run(agent
            .cli()
            .args(["join", invited["invite"].as_str().unwrap()]))
        .ok();
        agent
    }

    fn cli(&self) -> Command {
        let mut command = openboard();
        command.env("OPENBOARD_CONFIG_DIR", self.home.path().join("config"));
        command
    }

    fn post(&self, channel: &str, text: &str) -> Output {
        run(self.cli().args(["--json", "post", channel, text]))
    }

    /// The texts of the roots of `channel`, as `read --json` returns them.
    fn texts(&self, channel: &str) -> Vec<String> {
        let read: Value = run(self.cli().args(["--json", "read", channel]))
            .ok()
            .json();
        let threads = read["threads"]
            .as_array()
            .unwrap_or_else(|| panic!("{read}"));
        threads
            .iter()
            .map(|thread| thread["root"]["text"].as_str().unwrap().to_owned())
            .collect()
    }
}

/// The built binary with a clean environment and no stdin. Without a
/// configuration directory of its own it gets one that does not exist.
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

/// `openboard <args> --data-dir <data_dir>`, for the operator commands.
fn admin(data_dir: &Path, args: &[&str]) -> Output {
    run(openboard().args(args).arg("--data-dir").arg(data_dir))
}

#[derive(Debug, Clone)]
struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    /// Stdout as exactly one JSON document on one line.
    #[track_caller]
    fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        assert_eq!(
            self.stdout.trim_end_matches('\n').lines().count(),
            1,
            "expected one line of JSON on stdout: {self:?}"
        );
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not the expected JSON ({error}): {self:?}"))
    }

    #[track_caller]
    fn ok(self) -> Self {
        assert_eq!(self.code, Some(0), "{self:?}");
        self
    }

    /// A failure with this exit code and error code, in `--json` form.
    #[track_caller]
    fn error(&self, exit: i32, code: &str) -> openboard_api::ApiError {
        assert_eq!(self.code, Some(exit), "{self:?}");
        let body: ErrorBody = self.json();
        assert_eq!(body.error.code, code, "{self:?}");
        assert!(self.stderr.contains(&format!("error ({code})")), "{self:?}");
        body.error
    }
}

/// Runs a command to its end, failing if it takes more than a minute.
#[track_caller]
fn run(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the command did not exit within a minute: {command:?}");
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

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The names in a directory, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Checks a backup file the way a restore relies on it: it is private, it
/// is a whole SQLite database at the schema `admin backup` reported, and
/// the event log of every board in it verifies. Returns how many messages
/// it holds.
fn check_backup(written: &Value) -> i64 {
    let path = PathBuf::from(written["path"].as_str().unwrap());
    assert_eq!(mode_of(&path), 0o600, "{written}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        written["bytes"].as_u64().unwrap(),
        "{written}"
    );
    let messages = {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(check, "ok", "{written}");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, written["schema_version"].as_i64().unwrap());
        conn.query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap()
    };
    // Its hash chains are whole: no event is missing or half written.
    let scratch = TempDir::new().unwrap();
    std::fs::copy(&path, scratch.path().join("openboard.db")).unwrap();
    let verified: Value = admin(scratch.path(), &["--json", "admin", "verify-log"])
        .ok()
        .json();
    assert_eq!(verified["ok"], true, "{verified}");
    messages
}

// ---------------------------------------------------------------------------
// Taking a backup
// ---------------------------------------------------------------------------

#[test]
fn a_backup_taken_while_agents_post_is_consistent() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let _board = Served::start(&data_dir, 0, &["--open"]);
    let agents: Vec<Arc<Agent>> = ["agent-a", "agent-b"]
        .iter()
        .map(|name| Arc::new(Agent::enrol(&data_dir, name)))
        .collect();
    agents[0].post("notes", "the first message").ok();

    // Both agents post as fast as they can while the backups are taken.
    let stop = Arc::new(AtomicBool::new(false));
    let posted = Arc::new(AtomicUsize::new(0));
    let writers: Vec<_> = agents
        .iter()
        .enumerate()
        .map(|(n, agent)| {
            let (agent, stop, posted) = (agent.clone(), stop.clone(), posted.clone());
            std::thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    agent.post("notes", &format!("writer {n} message {i}")).ok();
                    posted.fetch_add(1, Ordering::Relaxed);
                    i += 1;
                }
            })
        })
        .collect();
    let backups = dir.path().join("backups");
    std::fs::create_dir(&backups).unwrap();
    let mut written = Vec::new();
    for _ in 0..4 {
        // Wait for some writes between one backup and the next.
        let before = posted.load(Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(30);
        while posted.load(Ordering::Relaxed) < before + 3 {
            assert!(Instant::now() < deadline, "the writers stalled");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Each into a file of its own, since names are to the second.
        let to = backups.join(format!("copy-{}.db", written.len()));
        let out = admin(
            &data_dir,
            &["--json", "admin", "backup", "--to", to.to_str().unwrap()],
        )
        .ok();
        let document: Value = out.json();
        assert_eq!(document["path"], to.to_str().unwrap());
        assert!(document["created_at"].as_str().unwrap().ends_with('Z'));
        written.push(document);
    }
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }

    // Every copy is whole, and later ones hold more.
    let counts: Vec<i64> = written.iter().map(check_backup).collect();
    assert!(counts[0] >= 1, "{counts:?}");
    assert!(
        counts.windows(2).all(|pair| pair[0] < pair[1]),
        "{counts:?}"
    );
}

#[test]
fn a_backup_is_private_never_replaces_a_file_and_leaves_nothing_when_it_fails() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut board = Served::start(&data_dir, 0, &[]);

    // By default: into the data directory, under a name of its own kind.
    let human = admin(&data_dir, &["admin", "backup"]).ok();
    assert!(human.stdout.starts_with("backup written: "), "{human:?}");
    for needle in [
        "readable only by its owner",
        "the text of every message, including messages deleted after it was taken",
        "keep tls-cert.pem and tls-key.pem",
        "docs/backup-and-restore.md",
    ] {
        assert!(human.stdout.contains(needle), "lacks {needle}: {human:?}");
    }
    let listed: Value = admin(&data_dir, &["--json", "admin", "backups"])
        .ok()
        .json();
    let rows = listed["backups"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["kind"], "on_demand");
    let name = rows[0]["file_name"].as_str().unwrap().to_owned();
    assert!(name.starts_with("openboard.db.backup-"), "{name}");
    assert!(name.ends_with('Z'), "{name}");
    assert_eq!(mode_of(&data_dir.join(&name)), 0o600);

    // A directory as --to gets a file of that name inside it.
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let into: Value = admin(
        &data_dir,
        &[
            "--json",
            "admin",
            "backup",
            "--to",
            elsewhere.to_str().unwrap(),
        ],
    )
    .ok()
    .json();
    let into_path = PathBuf::from(into["path"].as_str().unwrap());
    assert_eq!(into_path.parent().unwrap(), elsewhere);
    assert!(into_path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("openboard.db.backup-"));
    check_backup(&into);

    // A file that exists is left as it was, the database included.
    let taken = dir.path().join("taken.db");
    std::fs::write(&taken, "keep me").unwrap();
    for target in [taken.clone(), data_dir.join("openboard.db")] {
        let refused = admin(
            &data_dir,
            &[
                "--json",
                "admin",
                "backup",
                "--to",
                target.to_str().unwrap(),
            ],
        );
        let error = refused.error(1, "file_exists");
        assert!(error.next.is_some(), "{refused:?}");
    }
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "keep me");

    // A directory nobody may write to, or one that does not exist: a clear
    // error, and no file.
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root may write anywhere; the check means nothing then.
    let writable = std::fs::write(locked.join("probe"), "").is_ok();
    if !writable {
        let refused = admin(
            &data_dir,
            &[
                "--json",
                "admin",
                "backup",
                "--to",
                locked.to_str().unwrap(),
            ],
        );
        let error = refused.error(1, "operator_failed");
        assert!(error.message.contains("may not create files"), "{error:?}");
        assert!(error.next.unwrap().contains("--to"));
        assert!(names_in(&locked).is_empty());
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let missing = dir.path().join("no-such-dir").join("board.db");
    let refused = admin(
        &data_dir,
        &[
            "--json",
            "admin",
            "backup",
            "--to",
            missing.to_str().unwrap(),
        ],
    );
    let error = refused.error(1, "operator_failed");
    assert!(error.message.contains("does not exist"), "{error:?}");

    // --prune deletes the pre-migration backups and keeps the on-demand
    // one.
    std::fs::write(
        data_dir.join("openboard.db.pre-v5-20261119T120000Z"),
        vec![7u8; 4096],
    )
    .unwrap();
    let human = admin(&data_dir, &["admin", "backups"]).ok();
    assert!(human.stdout.contains("pre-migration"), "{human:?}");
    assert!(human.stdout.contains("on-demand"), "{human:?}");
    let pruned: Value = admin(&data_dir, &["--json", "admin", "backups", "--prune"])
        .ok()
        .json();
    let gone = pruned["backups"].as_array().unwrap();
    assert_eq!(gone.len(), 1, "{pruned}");
    assert_eq!(gone[0]["kind"], "pre_migration");
    assert!(data_dir.join(&name).exists());
    let human = admin(&data_dir, &["admin", "backups", "--prune"]).ok();
    assert!(human.stdout.contains("1 on-demand backup(s)"), "{human:?}");

    // A stopped board is backed up the same way.
    board.stop();
    let stopped: Value = admin(
        &data_dir,
        &[
            "--json",
            "admin",
            "backup",
            "--to",
            dir.path().join("stopped.db").to_str().unwrap(),
        ],
    )
    .ok()
    .json();
    check_backup(&stopped);

    // A database newer than this openboard is refused, as the other
    // operator commands refuse it, and nothing is written.
    let newer = dir.path().join("newer");
    std::fs::create_dir(&newer).unwrap();
    std::fs::copy(data_dir.join("openboard.db"), newer.join("openboard.db")).unwrap();
    {
        let conn = rusqlite::Connection::open(newer.join("openboard.db")).unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
    }
    let refused = admin(&newer, &["--json", "admin", "backup"]);
    let error = refused.error(1, "operator_failed");
    assert!(error.message.contains("schema version 99"), "{error:?}");
    // Opening it may leave SQLite's -wal and -shm files, as any operator
    // command does, but no backup.
    let names = names_in(&newer);
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("openboard.db.backup-")),
        "{names:?}"
    );
}

// ---------------------------------------------------------------------------
// Restoring, as docs/backup-and-restore.md says
// ---------------------------------------------------------------------------

#[test]
fn a_board_restored_from_a_backup_keeps_its_agents_and_messages() {
    let dir = TempDir::new().unwrap();
    let old_dir = dir.path().join("old-machine");
    let off_machine = dir.path().join("off-machine");
    std::fs::create_dir(&off_machine).unwrap();
    let port = free_port();

    // A board with a self-signed certificate, an agent and some messages.
    let mut board = Served::start(&old_dir, port, &[]);
    let agent = Agent::enrol(&old_dir, "agent-a");
    admin(&old_dir, &["admin", "grant", "*", "agent-a", "all"]).ok();
    agent.post("notes", "written before the backup").ok();
    agent.post("notes", "also before the backup").ok();

    // Taking the backup while the board runs, and copying it, with the
    // certificate and its key, off the machine.
    let written: Value = admin(
        &old_dir,
        &[
            "--json",
            "admin",
            "backup",
            "--to",
            off_machine.to_str().unwrap(),
        ],
    )
    .ok()
    .json();
    check_backup(&written);
    let backup = PathBuf::from(written["path"].as_str().unwrap());
    for file in ["tls-cert.pem", "tls-key.pem"] {
        std::fs::copy(old_dir.join(file), off_machine.join(file)).unwrap();
    }
    agent.post("notes", "written after the backup").ok();

    // The machine is lost.
    board.stop();
    std::fs::remove_dir_all(&old_dir).unwrap();

    // On the new machine: a private data directory, the backup as
    // openboard.db with nothing beside it, and the certificate and key.
    let new_dir = dir.path().join("new-machine");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&new_dir)
        .unwrap();
    std::fs::copy(&backup, new_dir.join("openboard.db")).unwrap();
    for file in ["tls-cert.pem", "tls-key.pem"] {
        std::fs::copy(off_machine.join(file), new_dir.join(file)).unwrap();
    }
    for file in ["openboard.db", "tls-cert.pem", "tls-key.pem"] {
        std::fs::set_permissions(new_dir.join(file), std::fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    assert_eq!(
        names_in(&new_dir),
        ["openboard.db", "tls-cert.pem", "tls-key.pem"]
    );

    // Started with the same flags at the same address, it is the same
    // board to its agents.
    let restored = Served::start(&new_dir, port, &[]);
    assert_eq!(restored.url, board.url);
    assert_eq!(restored.fingerprint, board.fingerprint);
    let info: Value = admin(&new_dir, &["--json", "admin", "info"]).ok().json();
    assert_eq!(info["mode"], "network");
    assert_eq!(info["url"], board.url.as_str());
    assert_eq!(info["fingerprint"], board.fingerprint.as_str());
    assert_eq!(info["would_migrate"], false);
    let verified: Value = admin(&new_dir, &["--json", "admin", "verify-log"])
        .ok()
        .json();
    assert_eq!(verified["ok"], true, "{verified}");

    // The agent's token from before still works, for reading and writing,
    // and the board holds what it held when the backup was taken.
    let texts = agent.texts("notes");
    assert!(
        texts.contains(&"written before the backup".to_owned()),
        "{texts:?}"
    );
    assert!(
        texts.contains(&"also before the backup".to_owned()),
        "{texts:?}"
    );
    assert!(
        !texts.contains(&"written after the backup".to_owned()),
        "{texts:?}"
    );
    agent.post("notes", "written after the restore").ok();
    assert!(agent
        .texts("notes")
        .contains(&"written after the restore".to_owned()));
}
