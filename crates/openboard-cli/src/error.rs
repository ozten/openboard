//! Failures as the CLI reports them: a named code, a message, a suggested
//! next step and an exit code.

use serde::Serialize;

/// Exit code for a failed command: an API error, a timeout, a broken log.
pub const EXIT_FAILURE: u8 = 1;
/// Exit code for a command line the CLI could not act on.
pub const EXIT_USAGE: u8 = 2;

/// Error codes the CLI adds to the server's `openboard_api::codes`.
pub mod codes {
    /// The command line or the environment is wrong. Exit code 2.
    pub const USAGE: &str = "usage";
    /// No board answered at the configured URL.
    pub const CONNECTION_FAILED: &str = "connection_failed";
    /// The board accepted the connection but did not answer in time.
    pub const REQUEST_TIMEOUT: &str = "request_timeout";
    /// The request failed in transit for another reason.
    pub const REQUEST_FAILED: &str = "request_failed";
    /// The answer was not what an Openboard server sends.
    pub const BAD_RESPONSE: &str = "bad_response";
    /// `wait` saw no matching event before its timeout.
    pub const TIMEOUT: &str = "timeout";
    /// SIGINT or SIGTERM stopped the command.
    pub const INTERRUPTED: &str = "interrupted";
    /// `admin verify-log` found a broken hash chain.
    pub const LOG_BROKEN: &str = "log_broken";
    /// `admin verify-log` could not read the data directory.
    pub const VERIFY_FAILED: &str = "verify_failed";
    /// `serve` was asked to listen on a non-loopback address.
    pub const NON_LOOPBACK_BIND: &str = "non_loopback_bind";
    /// `serve` could not start or stopped with a failure.
    pub const SERVE_FAILED: &str = "serve_failed";
    /// Reading stdin or writing output failed.
    pub const IO: &str = "io";
    /// The board did not prove it is the one this client pinned: it showed
    /// another certificate, or could not sign with the pinned one's key.
    pub const CERTIFICATE_MISMATCH: &str = "certificate_mismatch";
    /// Several stored logins match and none was chosen. Exit code 2.
    pub const AMBIGUOUS_LOGIN: &str = "ambiguous_login";
    /// The login file can be read by other users.
    pub const INSECURE_LOGIN_FILE: &str = "insecure_login_file";
    /// The login file cannot be read, parsed or written.
    pub const LOGIN_FILE: &str = "login_file";
    /// An operator command was run on a data directory that is not a
    /// network board yet.
    pub const NOT_NETWORK: &str = "not_network";
    /// An operator command could not use the data directory.
    pub const OPERATOR_FAILED: &str = "operator_failed";
}

/// A failed command.
#[derive(Debug, Clone)]
pub struct Failure {
    pub code: String,
    pub message: String,
    /// What the caller can do about it.
    pub next: Option<String>,
    /// Where `wait` or `tail` can resume, when the command got that far.
    pub cursor: Option<String>,
    /// Seconds the board asked the caller to wait, on `rate_limited`.
    pub retry_after: Option<u64>,
    pub exit: u8,
    /// The command already wrote its stdout document (or must not write one),
    /// so no JSON error object follows.
    pub no_stdout: bool,
    /// The command already explained itself on stderr.
    pub no_stderr: bool,
}

impl Failure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            next: None,
            cursor: None,
            retry_after: None,
            exit: EXIT_FAILURE,
            no_stdout: false,
            no_stderr: false,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        let mut failure = Self::new(codes::USAGE, message);
        failure.exit = EXIT_USAGE;
        failure
    }

    pub fn next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }

    /// Attaches a resume point: the cursor, and the command that continues
    /// from it. The command replaces any earlier hint, because resuming is
    /// the most useful thing to do after a failure part-way through a wait.
    pub fn resume(mut self, resume: Option<&Resume>) -> Self {
        if let Some(resume) = resume {
            self.cursor = Some(resume.cursor.clone());
            self.next = Some(match self.next.take() {
                // Keep a hint that fixes the cause, and add how to resume.
                Some(fix) if self.code != codes::TIMEOUT && self.code != codes::INTERRUPTED => {
                    format!("{fix}; then resume with `{}`", resume.command)
                }
                _ => format!("resume with `{}`", resume.command),
            });
        }
        self
    }

    pub fn exit(mut self, exit: u8) -> Self {
        self.exit = exit;
        self
    }

    /// The JSON document written to stdout under `--json`.
    pub fn to_json(&self) -> String {
        #[derive(Serialize)]
        struct Detail<'a> {
            code: &'a str,
            message: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            next: Option<&'a str>,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            error: Detail<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cursor: Option<&'a str>,
        }
        serde_json::to_string(&Body {
            error: Detail {
                code: &self.code,
                message: &self.message,
                next: self.next.as_deref(),
            },
            cursor: self.cursor.as_deref(),
        })
        .unwrap_or_else(|_| r#"{"error":{"code":"io","message":"cannot encode the error"}}"#.into())
    }

    /// The lines written to stderr.
    pub fn to_human(&self) -> String {
        let mut text = format!("openboard: error ({}): {}", self.code, self.message);
        if let Some(cursor) = &self.cursor {
            text.push_str(&format!("\ncursor: {cursor}"));
        }
        if let Some(next) = &self.next {
            text.push_str(&format!("\nnext: {next}"));
        }
        text
    }
}

/// A point `wait` or `tail` can continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub cursor: String,
    /// The full command line that resumes from `cursor`.
    pub command: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_error_matches_the_api_error_shape() {
        let failure = Failure::new(codes::TIMEOUT, "no event").resume(Some(&Resume {
            cursor: "ev_1".into(),
            command: "openboard wait --after ev_1".into(),
        }));
        let body: openboard_api::ErrorBody = serde_json::from_str(&failure.to_json()).unwrap();
        assert_eq!(body.error.code, "timeout");
        assert_eq!(
            body.error.next.as_deref(),
            Some("resume with `openboard wait --after ev_1`")
        );
        let raw: serde_json::Value = serde_json::from_str(&failure.to_json()).unwrap();
        assert_eq!(raw["cursor"], "ev_1");
    }

    #[test]
    fn a_fix_hint_is_kept_beside_the_resume_command() {
        let failure = Failure::new(codes::CONNECTION_FAILED, "down")
            .next("start a board")
            .resume(Some(&Resume {
                cursor: "ev_1".into(),
                command: "openboard wait --after ev_1".into(),
            }));
        assert_eq!(
            failure.next.as_deref(),
            Some("start a board; then resume with `openboard wait --after ev_1`")
        );
    }
}
