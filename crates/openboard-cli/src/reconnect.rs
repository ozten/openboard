//! Riding out an outage: what `wait`, `tail` and a keyed `post` do when the
//! board cannot be reached for a while, as when its server is restarted
//! for an upgrade.
//!
//! A failure is worth retrying when the board did not answer at all (the
//! connection was refused, dropped or timed out, or the TLS handshake did
//! not complete because nothing was there) or answered with a 5xx. It is
//! never retried when the board answered and refused: `unauthenticated`,
//! `forbidden`, `not_found`. And it is never retried when the server did
//! not prove it is the pinned board, `certificate_mismatch`: that is a
//! security failure, not an outage.

use std::time::{Duration, Instant};

use crate::cli;
use crate::error::{codes, Failure};

/// How long `wait` and `tail` keep retrying by default.
pub const DEFAULT_RECONNECT: &str = "2m";

/// Pauses between attempts. Short at first, because a server that is
/// restarted is usually back within a second or two.
const PAUSES_MS: [u64; 6] = [250, 500, 1000, 2000, 4000, 5000];

/// Whether trying again later may succeed.
pub fn transient(failure: &Failure) -> bool {
    let transport = [
        codes::CONNECTION_FAILED,
        codes::REQUEST_TIMEOUT,
        codes::REQUEST_FAILED,
    ]
    .contains(&failure.code.as_str());
    transport || failure.status.is_some_and(|status| status >= 500)
}

/// One command's patience with a board that has gone away.
#[derive(Debug)]
pub struct Outage {
    /// The longest run of failures to sit through. Zero retries nothing.
    limit: Duration,
    /// Whether to retry at all yet; see [`Outage::arm`].
    armed: bool,
    /// When the current run of failures began.
    began: Option<Instant>,
    attempts: usize,
}

impl Outage {
    /// Not armed: until the board has answered once, a failure is reported
    /// at once, so that a wrong URL or a board that is not running does not
    /// look like a wait.
    pub fn new(limit: Duration) -> Self {
        Self {
            limit,
            armed: false,
            began: None,
            attempts: 0,
        }
    }

    /// Retry from the first failure on, for a command that is known to
    /// continue something: it resumes from a cursor, or it repeats a post
    /// that carries an idempotency key.
    pub fn arm(mut self, armed: bool) -> Self {
        self.armed = armed;
        self
    }

    /// Whether the board has been failing since the last answer: the next
    /// request is a retry. A long-poll then asks without holding, so that
    /// the board's answer, and the line saying it is back, come at once.
    pub fn failing(&self) -> bool {
        self.began.is_some()
    }

    /// The board answered. Ends the outage, if there was one, with one line
    /// on stderr, and arms retrying from here on.
    pub fn recovered(&mut self) {
        self.armed = true;
        self.attempts = 0;
        if let Some(began) = self.began.take() {
            eprintln!(
                "openboard: reconnected to the board after {}s; continuing",
                began.elapsed().as_secs()
            );
        }
    }

    /// A request failed. Returns after a pause when it is worth trying
    /// again, and hands the failure back when it is not: it is not
    /// transient, retrying is off, or the outage has lasted [`Outage::new`]'s
    /// limit or would run past `deadline`. The first retry of an outage
    /// says so in one line on stderr.
    pub async fn ride(
        &mut self,
        failure: Failure,
        deadline: Option<Instant>,
    ) -> Result<(), Failure> {
        if !self.armed || self.limit.is_zero() || !transient(&failure) {
            return Err(failure);
        }
        let now = Instant::now();
        let began = *self.began.get_or_insert(now);
        let give_up = began + self.limit;
        let end = deadline.map_or(give_up, |deadline| deadline.min(give_up));
        if now >= end {
            let mut failure = failure;
            failure.message = format!(
                "{} (still failing after {}s of retrying)",
                failure.message,
                began.elapsed().as_secs()
            );
            return Err(failure);
        }
        if self.attempts == 0 {
            eprintln!(
                "openboard: {}; retrying for up to {}",
                failure.message,
                cli::format_duration(self.limit)
            );
        }
        let pause = Duration::from_millis(PAUSES_MS[self.attempts.min(PAUSES_MS.len() - 1)]);
        self.attempts += 1;
        tokio::time::sleep(pause.min(end - now)).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: &str, status: Option<u16>) -> Failure {
        let mut failure = Failure::new(code, "it failed");
        failure.status = status;
        failure
    }

    #[test]
    fn only_an_absent_or_failing_board_is_worth_retrying() {
        for code in [
            codes::CONNECTION_FAILED,
            codes::REQUEST_TIMEOUT,
            codes::REQUEST_FAILED,
        ] {
            assert!(transient(&failure(code, None)), "{code}");
        }
        assert!(transient(&failure(
            openboard_api::codes::INTERNAL,
            Some(500)
        )));
        assert!(transient(&failure(codes::BAD_RESPONSE, Some(502))));
        for (code, status) in [
            (openboard_api::codes::UNAUTHENTICATED, Some(401)),
            (openboard_api::codes::FORBIDDEN, Some(403)),
            (openboard_api::codes::NOT_FOUND, Some(404)),
            (openboard_api::codes::RATE_LIMITED, Some(429)),
            (codes::CERTIFICATE_MISMATCH, None),
            (codes::BAD_RESPONSE, Some(200)),
            (codes::USAGE, None),
        ] {
            assert!(!transient(&failure(code, status)), "{code}");
        }
    }

    #[tokio::test]
    async fn an_outage_is_ridden_only_when_armed_and_only_for_so_long() {
        let down = || failure(codes::CONNECTION_FAILED, None);
        // Not armed: the first contact must succeed.
        let mut outage = Outage::new(Duration::from_secs(60));
        assert!(outage.ride(down(), None).await.is_err());
        // Armed by an answer, it retries, and never what the board refused.
        outage.recovered();
        assert!(outage.ride(down(), None).await.is_ok());
        let refused = failure(openboard_api::codes::UNAUTHENTICATED, Some(401));
        assert!(outage.ride(refused, None).await.is_err());
        let pinned = failure(codes::CERTIFICATE_MISMATCH, None);
        assert!(outage.ride(pinned, None).await.is_err());
        // A limit of zero turns retrying off.
        let mut off = Outage::new(Duration::ZERO).arm(true);
        assert!(off.ride(down(), None).await.is_err());
        // A deadline that has passed ends it, and the failure says so.
        let mut late = Outage::new(Duration::from_secs(60)).arm(true);
        let gave_up = late.ride(down(), Some(Instant::now())).await.unwrap_err();
        assert_eq!(gave_up.code, codes::CONNECTION_FAILED);
        assert!(gave_up.message.contains("still failing"), "{gave_up:?}");
        // So does the limit itself.
        let mut brief = Outage::new(Duration::from_millis(300)).arm(true);
        let started = Instant::now();
        let gave_up = loop {
            match brief.ride(down(), None).await {
                Ok(()) => assert!(started.elapsed() < Duration::from_secs(5)),
                Err(failure) => break failure,
            }
        };
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(gave_up.message.contains("still failing"), "{gave_up:?}");
    }
}
