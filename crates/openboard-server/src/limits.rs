//! In-memory rate limits (design A13, O7, O9).
//!
//! Counters live in fixed one-minute windows. Every table has a hard cap on
//! its number of keys: when it is full, stale windows are swept, and if it
//! is still full a new key is refused rather than tracked, so an attacker
//! who invents addresses cannot grow the server's memory.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Rate limits, connection limits and timeouts.
///
/// The request, enrolment, authentication and waiter limits apply to a
/// network board; a local board has no credentials to count them against.
/// The connection limits and timeouts apply to both.
///
/// The defaults are far above what a few dozen agents that post, read and
/// wait will reach, also when many of them share one client address, as
/// agents on the board's own host do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Requests one credential may make per minute, shared by all of its
    /// profiles.
    pub requests_per_minute: u32,
    /// Failed enrolment attempts (`POST /v1/join`) one client address may
    /// make per minute. Past that, further failures are answered with
    /// `rate_limited`. A valid code is always accepted and never counted,
    /// so junk from one address cannot lock out an agent that shares it.
    pub joins_per_minute: u32,
    /// Requests with a missing or bad token one client address may make per
    /// minute before it is told to slow down instead.
    pub auth_failures_per_minute: u32,
    /// Long-polls plus event streams one credential may hold open at once.
    pub max_waiters: u32,
    /// The longest a long-poll or event stream goes without re-reading its
    /// credential, so that a revocation or an expiry ends it.
    ///
    /// An event is only sent when the credential was read as usable within
    /// this interval, and after any time spent waiting for a new event it
    /// is read again first, whatever this is set to. A stream whose reader
    /// has stalled is ended by the same check. What a revoked credential
    /// can still receive is therefore: events posted up to this long after
    /// the revocation while its stream was catching up on a backlog, and
    /// the few frames that had already been handed to the connection.
    pub credential_recheck: Duration,
    /// A credential's last-used time is rewritten at most this often, and
    /// so is a seat's last-seen time on a board.
    pub last_used_interval: Duration,
    /// Presence: a seat with no request open is live for this long after
    /// its last request.
    pub presence_live: Duration,
    /// Presence: after that, it is recent for this long after its last
    /// request, and away from then on.
    pub presence_recent: Duration,
    /// Presence: a seat not seen for this long leaves the roster, and its
    /// last-seen time is deleted.
    pub presence_listed: Duration,
    /// Connections open at once, over all clients, from the moment they
    /// are accepted. Further connections are closed at once.
    /// The default stays under the common 256 open-file limit, which the
    /// server does not raise.
    pub max_connections: u32,
    /// Connections one client address may have open at once. Further ones
    /// are closed at once. An IPv6 address counts by its /64.
    pub max_connections_per_address: u32,
    /// Network mode: connections of one client address that may be in
    /// their TLS handshake at once, so that an address that opens
    /// connections and says nothing cannot use up its other places.
    pub max_handshakes_per_address: u32,
    /// Network mode: a client must finish its TLS handshake within this
    /// time.
    pub handshake_timeout: Duration,
    /// A client must send the complete head of a request within this time,
    /// counted from the handshake or from the end of the previous response,
    /// so it also closes a connection left idle between requests. The body
    /// of an enrolment request has the same time. A long-poll or a stream
    /// that is being answered is not affected, however long it lasts.
    pub header_read_timeout: Duration,
    /// Failed authentications and enrolments written to the security log
    /// per minute, over all clients. The rest of the minute is summarised
    /// by one `log.suppressed` entry, which bounds how fast failures can
    /// grow the log.
    pub failure_log_per_minute: u32,
    /// The share of [`Limits::failure_log_per_minute`] one client address
    /// can use, so that a few addresses cannot silence the entries of
    /// everyone else. An address over its share gets one `log.suppressed`
    /// entry of its own for the minute.
    pub failure_log_per_address_per_minute: u32,
}

impl Default for Limits {
    /// 1200 requests a minute per credential, 10 failed enrolments and 30
    /// failed authentications a minute per client address, 32 open
    /// long-polls and streams per credential, re-checked every 5 seconds,
    /// last-used times a minute apart; presence live for 2 minutes after a
    /// request, recent for 15 and listed for 30 days; 200 connections, 128 per client
    /// address of which 48 in their handshake, 3 seconds for a handshake
    /// and 10 for a request head; 240 logged failures a minute, 20 per
    /// client address.
    fn default() -> Self {
        Self {
            requests_per_minute: 1200,
            joins_per_minute: 10,
            auth_failures_per_minute: 30,
            max_waiters: 32,
            credential_recheck: Duration::from_secs(5),
            last_used_interval: Duration::from_secs(60),
            presence_live: Duration::from_secs(120),
            presence_recent: Duration::from_secs(15 * 60),
            presence_listed: Duration::from_secs(30 * 24 * 60 * 60),
            max_connections: 200,
            max_connections_per_address: 128,
            max_handshakes_per_address: 48,
            handshake_timeout: Duration::from_secs(3),
            header_read_timeout: Duration::from_secs(10),
            failure_log_per_minute: 240,
            failure_log_per_address_per_minute: 20,
        }
    }
}

/// How long the security log is kept.
///
/// Entries beyond either bound are removed, oldest first, when the server
/// starts and every [`LogRetention::check_every`] while it runs. Each
/// removal leaves one `log.pruned` entry with the number removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRetention {
    /// Entries older than this are removed.
    pub max_age: Duration,
    /// At most this many entries are kept.
    pub max_entries: u64,
    /// How often a running server enforces the two bounds.
    pub check_every: Duration,
}

impl Default for LogRetention {
    /// 90 days, 500 000 entries, enforced every 10 minutes.
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(90 * 24 * 60 * 60),
            max_entries: 500_000,
            check_every: Duration::from_secs(10 * 60),
        }
    }
}

const WINDOW: Duration = Duration::from_secs(60);
/// Most keys a table tracks at once.
const MAX_KEYS: usize = 16_384;

struct Slot {
    started: Instant,
    count: u32,
}

/// The outcome of counting one event against a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Allowed,
    Limited {
        /// Seconds until the window ends, at least 1.
        retry_after: u64,
        /// True the first time this key goes over in this window.
        first: bool,
    },
}

#[cfg(test)]
impl Hit {
    pub(crate) fn is_allowed(self) -> bool {
        self == Hit::Allowed
    }
}

pub(crate) struct RateTable<K> {
    max_keys: usize,
    slots: Mutex<HashMap<K, Slot>>,
}

impl<K: Hash + Eq> RateTable<K> {
    pub(crate) fn new() -> Self {
        Self::with_capacity(MAX_KEYS)
    }

    fn with_capacity(max_keys: usize) -> Self {
        Self {
            max_keys,
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one event for `key`. The first `limit` events in a window are
    /// allowed.
    pub(crate) fn hit(&self, key: K, limit: u32) -> Hit {
        self.hit_at(key, limit, Instant::now())
    }

    fn hit_at(&self, key: K, limit: u32, now: Instant) -> Hit {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let fresh = |slot: &Slot| now.duration_since(slot.started) < WINDOW;
        if !slots.contains_key(&key) && slots.len() >= self.max_keys {
            slots.retain(|_, slot| fresh(slot));
            if slots.len() >= self.max_keys {
                return Hit::Limited {
                    retry_after: WINDOW.as_secs(),
                    first: false,
                };
            }
        }
        let slot = slots.entry(key).or_insert(Slot {
            started: now,
            count: 0,
        });
        if !fresh(slot) {
            *slot = Slot {
                started: now,
                count: 0,
            };
        }
        slot.count = slot.count.saturating_add(1);
        if slot.count <= limit {
            Hit::Allowed
        } else {
            let left = WINDOW.saturating_sub(now.duration_since(slot.started));
            Hit::Limited {
                retry_after: left.as_secs_f64().ceil().max(1.0) as u64,
                first: slot.count == limit.saturating_add(1),
            }
        }
    }
}

/// The key a client address is limited under: an IPv4 address as it is, an
/// IPv6 address by its /64, because one host usually controls a whole /64.
pub(crate) fn address_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            let [a, b, c, d, ..] = v6.segments();
            IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
        }
    }
}

/// Counts the long-polls and streams each credential holds open. It has one
/// entry per credential with something open, so it is bounded by the number
/// of open connections.
#[derive(Default)]
pub(crate) struct Waiters {
    open: Mutex<HashMap<String, u32>>,
}

impl Waiters {
    /// Reserves a place for `credential_id`, or returns `None` when it
    /// already holds `max`. The place is given back when the guard drops.
    pub(crate) fn acquire(self: &Arc<Self>, credential_id: &str, max: u32) -> Option<WaiterGuard> {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let count = open.get(credential_id).copied().unwrap_or(0);
        if count >= max {
            return None;
        }
        open.insert(credential_id.to_owned(), count + 1);
        Some(WaiterGuard {
            waiters: Arc::clone(self),
            credential_id: credential_id.to_owned(),
        })
    }
}

pub(crate) struct WaiterGuard {
    waiters: Arc<Waiters>,
    credential_id: String,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut open = self
            .waiters
            .open
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match open.get_mut(&self.credential_id) {
            Some(count) if *count > 1 => *count -= 1,
            _ => {
                open.remove(&self.credential_id);
            }
        }
    }
}

/// Counts open connections, in total and per client address, and the TLS
/// handshakes each address has in progress. It has one entry per address
/// with a connection open, so it is bounded by [`Limits::max_connections`].
pub(crate) struct Connections {
    max: u32,
    max_per_address: u32,
    max_handshakes_per_address: u32,
    open: Mutex<ConnectionCounts>,
}

#[derive(Default)]
struct ConnectionCounts {
    total: u32,
    /// Per address: connections, and how many of them are in a handshake.
    by_address: HashMap<IpAddr, (u32, u32)>,
}

impl Connections {
    pub(crate) fn new(limits: &Limits) -> Arc<Self> {
        Arc::new(Self {
            max: limits.max_connections,
            max_per_address: limits.max_connections_per_address,
            max_handshakes_per_address: limits.max_handshakes_per_address,
            open: Mutex::new(ConnectionCounts::default()),
        })
    }

    fn counts(&self) -> std::sync::MutexGuard<'_, ConnectionCounts> {
        self.open
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Reserves a place for a new connection from `client`, or returns
    /// `None` when the board or the address is full. With `handshake`, the
    /// connection also takes one of the address's handshake places until
    /// [`ConnectionGuard::handshake_done`]. Everything is given back when
    /// the guard drops.
    pub(crate) fn admit(
        self: &Arc<Self>,
        client: IpAddr,
        handshake: bool,
    ) -> Option<ConnectionGuard> {
        let address = address_key(client);
        let mut counts = self.counts();
        if counts.total >= self.max {
            return None;
        }
        let (open, handshakes) = counts.by_address.get(&address).copied().unwrap_or((0, 0));
        if open >= self.max_per_address
            || (handshake && handshakes >= self.max_handshakes_per_address)
        {
            return None;
        }
        counts.total += 1;
        counts
            .by_address
            .insert(address, (open + 1, handshakes + u32::from(handshake)));
        Some(ConnectionGuard {
            connections: Arc::clone(self),
            address,
            handshaking: handshake,
        })
    }
}

pub(crate) struct ConnectionGuard {
    connections: Arc<Connections>,
    address: IpAddr,
    handshaking: bool,
}

impl ConnectionGuard {
    /// Gives back the handshake place. The connection keeps its own.
    pub(crate) fn handshake_done(&mut self) {
        if std::mem::take(&mut self.handshaking) {
            let mut counts = self.connections.counts();
            if let Some((_, handshakes)) = counts.by_address.get_mut(&self.address) {
                *handshakes = handshakes.saturating_sub(1);
            }
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.handshake_done();
        let mut counts = self.connections.counts();
        counts.total = counts.total.saturating_sub(1);
        match counts.by_address.get_mut(&self.address) {
            Some((open, _)) if *open > 1 => *open -= 1,
            _ => {
                counts.by_address.remove(&self.address);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_allows_the_limit_then_refuses_until_it_ends() {
        let table = RateTable::new();
        let start = Instant::now();
        assert!(table.hit_at("a", 2, start).is_allowed());
        assert!(table.hit_at("a", 2, start).is_allowed());
        assert_eq!(
            table.hit_at("a", 2, start + Duration::from_secs(10)),
            Hit::Limited {
                retry_after: 50,
                first: true
            }
        );
        assert_eq!(
            table.hit_at("a", 2, start + Duration::from_millis(59_500)),
            Hit::Limited {
                retry_after: 1,
                first: false
            }
        );
        // Another key is unaffected, and the window ends.
        assert!(table.hit_at("b", 2, start).is_allowed());
        assert!(table.hit_at("a", 2, start + WINDOW).is_allowed());
    }

    #[test]
    fn a_full_table_refuses_new_keys_until_windows_end() {
        let table = RateTable::with_capacity(2);
        let start = Instant::now();
        assert!(table.hit_at(1, 5, start).is_allowed());
        assert!(table.hit_at(2, 5, start).is_allowed());
        assert!(!table.hit_at(3, 5, start).is_allowed());
        // Known keys still count normally.
        assert!(table.hit_at(1, 5, start).is_allowed());
        // Stale windows are swept to make room.
        assert!(table.hit_at(3, 5, start + WINDOW).is_allowed());
        assert_eq!(table.slots.lock().unwrap().len(), 1);
    }

    #[test]
    fn addresses_are_keyed_by_host() {
        let key = |text: &str| address_key(text.parse().unwrap()).to_string();
        assert_eq!(key("10.0.0.7"), "10.0.0.7");
        assert_eq!(key("::ffff:10.0.0.7"), "10.0.0.7");
        assert_eq!(key("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2::");
    }

    #[test]
    fn waiters_are_capped_and_released() {
        let waiters = Arc::new(Waiters::default());
        let first = waiters.acquire("crd_1", 2).unwrap();
        let _second = waiters.acquire("crd_1", 2).unwrap();
        assert!(waiters.acquire("crd_1", 2).is_none());
        assert!(waiters.acquire("crd_2", 2).is_some());
        drop(first);
        assert!(waiters.acquire("crd_1", 2).is_some());
        assert_eq!(waiters.open.lock().unwrap().len(), 1);
    }

    #[test]
    fn connections_are_capped_in_total_per_address_and_per_handshake() {
        let limits = Limits {
            max_connections: 5,
            max_connections_per_address: 3,
            max_handshakes_per_address: 2,
            ..Limits::default()
        };
        let connections = Connections::new(&limits);
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        let a = ip("10.0.0.1");
        let mut first = connections.admit(a, true).unwrap();
        let _second = connections.admit(a, true).unwrap();
        // Two handshakes pending: a third is refused until one finishes.
        assert!(connections.admit(a, true).is_none());
        first.handshake_done();
        first.handshake_done();
        let third = connections.admit(a, true).unwrap();
        // Three connections: the address is full, with or without a
        // handshake, and whichever way the address is written.
        assert!(connections.admit(a, false).is_none());
        assert!(connections.admit(ip("::ffff:10.0.0.1"), false).is_none());
        // Others are unaffected, up to the total.
        let _b = connections.admit(ip("10.0.0.2"), false).unwrap();
        let _c = connections.admit(ip("2001:db8::1"), true).unwrap();
        assert!(connections.admit(ip("10.0.0.3"), false).is_none());
        drop(third);
        assert!(connections.admit(ip("10.0.0.3"), false).is_some());
        drop(first);
        let counts = connections.counts();
        assert_eq!(counts.total, 3);
        assert_eq!(counts.by_address.get(&a), Some(&(1, 1)));
        assert_eq!(counts.by_address.len(), 3);
    }
}
