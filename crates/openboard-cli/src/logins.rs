//! Stored logins: the tokens `openboard join` received, kept for later
//! commands.
//!
//! All logins live in one JSON file, [`FILE_NAME`], in the user's
//! configuration directory:
//!
//! 1. `$OPENBOARD_CONFIG_DIR`, if set;
//! 2. else `$XDG_CONFIG_HOME/openboard`, if that variable is an absolute path;
//! 3. else `~/.config/openboard` (on Windows, `%APPDATA%\openboard`).
//!
//! The file holds bearer tokens, so on Unix the directory is created with
//! mode 0700 and the file with mode 0600, and a file that its group or
//! anyone else can access is refused rather than read. It is replaced
//! atomically: written under a temporary name in the same directory, then
//! renamed. A token is never printed; [`Secret`] keeps it out of debug
//! output too.
//!
//! ```json
//! {"version":1,"logins":[{"url":"https://192.168.1.20:7878",
//!   "fingerprint":"sha256:…","principal":"agent-a","credential_id":"crd_…",
//!   "token":"obt_…","expires_at":"2027-02-16T10:00:00.000Z"}]}
//! ```
//!
//! A login is identified by its board URL and principal together, so two
//! agents under one OS user can both be enrolled in the same board.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{codes, Failure};

pub const FILE_NAME: &str = "logins.json";
const LOCK_NAME: &str = "logins.lock";
const VERSION: u32 = 1;

/// A bearer token. It can be sent and stored but not printed: `Debug` hides
/// it and there is no `Display`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The token itself, for the `Authorization` header and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    /// Base URL of the board, without a trailing slash.
    pub url: String,
    /// The pinned certificate fingerprint. Absent for a board whose
    /// certificate is checked against the certificate authorities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Handle of the principal the token authenticates.
    pub principal: String,
    pub credential_id: String,
    pub token: Secret,
    /// RFC 3339 timestamp in UTC.
    pub expires_at: String,
}

impl Login {
    pub fn is(&self, url: &str, principal: &str) -> bool {
        same_url(&self.url, url) && same_principal(&self.principal, principal)
    }
}

/// Board URLs are compared without a trailing slash and without regard to
/// ASCII case.
pub fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/')
        .eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// Handles are unique on a board without regard to case.
pub fn same_principal(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[derive(Serialize, Deserialize)]
struct Body {
    version: u32,
    logins: Vec<Login>,
}

/// The configuration directory, from the environment as `get` reports it.
fn config_dir_from(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| get(name).filter(|value| !value.is_empty());
    if let Some(dir) = set("OPENBOARD_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    if cfg!(windows) {
        return set("APPDATA").map(|dir| PathBuf::from(dir).join("openboard"));
    }
    if let Some(dir) = set("XDG_CONFIG_HOME").map(PathBuf::from) {
        // The XDG specification says a relative path here is ignored.
        if dir.is_absolute() {
            return Some(dir.join("openboard"));
        }
    }
    set("HOME").map(|home| PathBuf::from(home).join(".config").join("openboard"))
}

/// The configuration directory, whether or not it exists.
pub fn config_dir() -> Result<PathBuf, Failure> {
    file_path().map(|path| path.parent().map(Path::to_path_buf).unwrap_or_default())
}

/// Where the login file is, whether or not it exists.
pub fn file_path() -> Result<PathBuf, Failure> {
    config_dir_from(|name| std::env::var_os(name))
        .map(|dir| dir.join(FILE_NAME))
        .ok_or_else(|| {
            Failure::new(
                codes::LOGIN_FILE,
                "cannot tell where to keep logins: no home directory is set",
            )
            .next("set OPENBOARD_CONFIG_DIR to a directory only you can read")
        })
}

pub(crate) fn io_failure(what: &str, path: &Path, error: &std::io::Error) -> Failure {
    Failure::new(
        codes::LOGIN_FILE,
        format!("cannot {what} {}: {error}", path.display()),
    )
    .next("check the directory's permissions, or set OPENBOARD_CONFIG_DIR to one you can write")
}

/// The stored logins, as read from one file.
#[derive(Debug)]
pub struct Store {
    pub path: PathBuf,
    pub logins: Vec<Login>,
}

impl Store {
    /// Reads the login file. A missing file is an empty store.
    pub fn load() -> Result<Self, Failure> {
        Self::load_from(file_path()?)
    }

    pub fn load_from(path: PathBuf) -> Result<Self, Failure> {
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    logins: Vec::new(),
                })
            }
            Err(error) => return Err(io_failure("read", &path, &error)),
        };
        // Checked on the open file, so it is the file that is then read.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file
                .metadata()
                .map_err(|error| io_failure("read", &path, &error))?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                return Err(Failure::new(
                    codes::INSECURE_LOGIN_FILE,
                    format!(
                        "refusing to use the login file {}: its mode is {mode:04o}, so other \
                         users on this machine can access the tokens in it",
                        path.display()
                    ),
                )
                .next(format!(
                    "run `chmod 600 {}`; if anyone else may have read it, ask each board's \
                     operator to revoke your credential and invite you again",
                    crate::shell_quote(&path.display().to_string())
                )));
            }
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|error| io_failure("read", &path, &error))?;
        // The parser's own message can quote a value from the file, which
        // may be a token. Only the position is passed on.
        let damaged = |line: usize, column: usize| {
            Failure::new(
                codes::LOGIN_FILE,
                format!(
                    "the login file {} is not what this version writes (line {line}, column \
                     {column})",
                    path.display()
                ),
            )
            .next(
                "move the file aside and run `openboard join` again with a new invite from each \
                 board's operator",
            )
        };
        let body: Body =
            serde_json::from_str(&text).map_err(|error| damaged(error.line(), error.column()))?;
        if body.version != VERSION {
            return Err(Failure::new(
                codes::LOGIN_FILE,
                format!(
                    "the login file {} has format version {}, but this version of openboard \
                     reads version {VERSION}",
                    path.display(),
                    body.version
                ),
            )
            .next("use the newer openboard that wrote it, or move the file aside and join again"));
        }
        Ok(Self {
            path,
            logins: body.logins,
        })
    }

    /// Makes sure the login file can be read and its directory written,
    /// before something irreversible depends on saving to it.
    pub fn prepare() -> Result<Self, Failure> {
        let store = Self::load()?;
        create_private_dir(store.dir())?;
        Ok(store)
    }

    fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// Reads the file, applies `change` and writes the result back, holding
    /// a lock so that two commands changing it at once both take effect.
    pub fn update<T>(change: impl FnOnce(&mut Store) -> T) -> Result<(T, PathBuf), Failure> {
        let path = file_path()?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        create_private_dir(&dir)?;
        let lock_path = dir.join(LOCK_NAME);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        private(&mut options);
        let lock = options
            .open(&lock_path)
            .map_err(|error| io_failure("open", &lock_path, &error))?;
        match lock.lock() {
            Ok(()) => {}
            // Without lock support the update still happens, unguarded.
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {}
            Err(error) => return Err(io_failure("lock", &lock_path, &error)),
        }
        let mut store = Self::load_from(path)?;
        let result = change(&mut store);
        store.save()?;
        // The lock is released when `lock` is dropped here.
        Ok((result, store.path))
    }

    /// Replaces the login for the same board and principal, or adds one.
    /// Returns the login that was replaced.
    pub fn put(&mut self, login: Login) -> Option<Login> {
        let old = self.take(&login.url, &login.principal);
        self.logins.push(login);
        old
    }

    /// Removes and returns the login for this board and principal.
    pub fn take(&mut self, url: &str, principal: &str) -> Option<Login> {
        let at = self
            .logins
            .iter()
            .position(|login| login.is(url, principal))?;
        Some(self.logins.remove(at))
    }

    fn save(&self) -> Result<(), Failure> {
        let body = serde_json::to_vec_pretty(&Body {
            version: VERSION,
            logins: self.logins.clone(),
        })
        .map_err(|_| Failure::new(codes::LOGIN_FILE, "cannot encode the login file"))?;
        let temp = self
            .dir()
            .join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
        match std::fs::remove_file(&temp) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_failure("replace", &temp, &error)),
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        private(&mut options);
        let written = options.open(&temp).and_then(|mut file| {
            file.write_all(&body)?;
            file.write_all(b"\n")?;
            file.sync_all()
        });
        let moved = written.and_then(|()| std::fs::rename(&temp, &self.path));
        moved.map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            io_failure("write", &self.path, &error)
        })
    }
}

/// New files are readable and writable by their owner only.
pub(crate) fn private(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

/// Creates the configuration directory, private to its owner on Unix. An
/// existing directory is left as it is; the file's own mode protects it.
pub(crate) fn create_private_dir(dir: &Path) -> Result<(), Failure> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|error| io_failure("create", dir, &error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(url: &str, principal: &str, token: &str) -> Login {
        Login {
            url: url.into(),
            fingerprint: Some(openboard_api::auth::fingerprint(&[1; 32])),
            principal: principal.into(),
            credential_id: "crd_1".into(),
            token: Secret::new(token.into()),
            expires_at: "2027-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn the_config_directory_follows_the_documented_order() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            config_dir_from(env(&[
                ("OPENBOARD_CONFIG_DIR", "/x/ob"),
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/home/a"),
                ("APPDATA", "/appdata"),
            ])),
            Some(PathBuf::from("/x/ob"))
        );
        if cfg!(not(windows)) {
            assert_eq!(
                config_dir_from(env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/a")])),
                Some(PathBuf::from("/xdg/openboard"))
            );
            // Empty and relative values of XDG_CONFIG_HOME are ignored.
            for xdg in ["", "relative"] {
                let pairs: &'static [(&str, &str)] = if xdg.is_empty() {
                    &[("XDG_CONFIG_HOME", ""), ("HOME", "/home/a")]
                } else {
                    &[("XDG_CONFIG_HOME", "relative"), ("HOME", "/home/a")]
                };
                assert_eq!(
                    config_dir_from(env(pairs)),
                    Some(PathBuf::from("/home/a/.config/openboard"))
                );
            }
            assert_eq!(config_dir_from(env(&[])), None);
        }
    }

    #[test]
    fn a_token_never_shows_in_debug_output() {
        let login = login("https://b:1", "agent-a", "obt_supersecret");
        let shown = format!("{login:?}");
        assert!(!shown.contains("supersecret"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn logins_are_keyed_by_url_and_principal() {
        let mut store = Store {
            path: PathBuf::from("unused"),
            logins: Vec::new(),
        };
        assert!(store.put(login("https://b:1", "agent-a", "one")).is_none());
        assert!(store.put(login("https://b:1", "agent-b", "two")).is_none());
        assert!(store
            .put(login("https://c:1", "agent-a", "three"))
            .is_none());
        let old = store.put(login("https://B:1/", "Agent-A", "four")).unwrap();
        assert_eq!(old.token.expose(), "one");
        assert_eq!(store.logins.len(), 3);
        assert_eq!(
            store.take("https://b:1", "agent-a").unwrap().token.expose(),
            "four"
        );
        assert!(store.take("https://b:1", "agent-a").is_none());
    }

    #[test]
    fn a_damaged_file_is_reported_without_quoting_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        private(&mut options);
        options
            .open(&path)
            .unwrap()
            .write_all(br#"{"version":"obt_leakedtoken","logins":[]}"#)
            .unwrap();
        let failure = Store::load_from(path).unwrap_err();
        assert_eq!(failure.code, codes::LOGIN_FILE);
        assert!(!failure.message.contains("leakedtoken"), "{failure:?}");
    }
}
