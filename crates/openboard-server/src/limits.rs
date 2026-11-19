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

/// Rate limits of a network board. A local board has none.
///
/// The defaults are far above what an agent that posts, reads and waits
/// will reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Requests one credential may make per minute, shared by all of its
    /// profiles.
    pub requests_per_minute: u32,
    /// Enrolment attempts (`POST /v1/join`) one client address may make per
    /// minute, successful or not.
    pub joins_per_minute: u32,
    /// Requests with a missing or bad token one client address may make per
    /// minute before it is told to slow down instead.
    pub auth_failures_per_minute: u32,
    /// Long-polls plus event streams one credential may hold open at once.
    pub max_waiters: u32,
    /// How often an open long-poll or event stream re-reads its credential,
    /// so that a revocation or an expiry ends it. Events are never delivered
    /// without a fresh check, whatever this is set to.
    pub credential_recheck: Duration,
}

impl Default for Limits {
    /// 1200 requests a minute per credential, 10 enrolment attempts and 30
    /// failed authentications a minute per client address, 32 open
    /// long-polls and streams per credential, re-checked every 5 seconds.
    fn default() -> Self {
        Self {
            requests_per_minute: 1200,
            joins_per_minute: 10,
            auth_failures_per_minute: 30,
            max_waiters: 32,
            credential_recheck: Duration::from_secs(5),
        }
    }
}

const WINDOW: Duration = Duration::from_secs(60);
/// Most keys a table tracks at once.
const MAX_KEYS: usize = 16_384;
/// Failed authentications and enrolments written to the security log per
/// minute, over all clients. The rest of the minute is summarised by one
/// entry, which bounds how fast an attacker can grow the log.
pub(crate) const FAILURE_LOG_PER_MINUTE: u32 = 120;

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
}
