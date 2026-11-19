//! The authorization seam (design requirement Z13).
//!
//! Every operation the server performs on behalf of a caller goes through
//! [`Authorizer::authorize`]: request handlers, listings (once per item
//! returned) and event delivery in the long-poll and the stream (once per
//! event). Nothing else in the crate decides who may do what.
//!
//! In milestone M0 there is one principal, the local one, and it holds `all`,
//! so [`LocalPolicy`] allows everything it is asked. Later milestones replace
//! the [`Policy`], not the call sites.

use std::sync::Arc;

use openboard_api::{Actor, ActorKind};
use sha2::{Digest, Sha256};

use crate::chain::hex;

/// Who is making a request. Established by the server, never by the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Stable principal ID.
    pub id: String,
    pub kind: ActorKind,
    /// Display handle, without any profile.
    pub handle: String,
}

impl Principal {
    /// The single unauthenticated principal of a local-mode server.
    pub fn local() -> Self {
        Self {
            id: "local".to_owned(),
            kind: ActorKind::Local,
            handle: "local".to_owned(),
        }
    }

    /// The actor this principal writes as under an optional declared profile.
    ///
    /// The ID is derived from the principal ID and the profile alone, so the
    /// same pair always maps to the same actor.
    pub(crate) fn actor(&self, profile: Option<&str>) -> Actor {
        let mut hasher = Sha256::new();
        hasher.update(b"openboard.actor.v1\n");
        hasher.update(self.id.as_bytes());
        hasher.update(b"\n");
        if let Some(profile) = profile {
            hasher.update(b"profile\n");
            hasher.update(profile.as_bytes());
        }
        let digest = hex(&hasher.finalize());
        Actor {
            id: format!("act_{}", &digest[..24]),
            kind: self.kind,
            handle: match profile {
                Some(profile) => format!("{}/{profile}", self.handle),
                None => self.handle.clone(),
            },
            profile: profile.map(str::to_owned),
        }
    }
}

/// What the caller is trying to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Read the server's health and version. Carries no board.
    Health,
    /// Read threads, messages, channel names or events.
    Read,
    /// Post a thread or a reply.
    Post,
    /// Create a channel by posting to a path that does not exist yet.
    CreateChannel,
    /// Create a board by using a name that does not exist yet.
    CreateBoard,
}

/// The outcome of an authorization check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    pub fn is_allowed(self) -> bool {
        self == Decision::Allow
    }
}

/// The rules behind [`Authorizer::authorize`].
///
/// `board` is `None` only for actions that have no board. `channel` is `None`
/// when the action concerns the board as a whole, and otherwise the exact
/// path of one channel.
pub trait Policy: Send + Sync + 'static {
    fn decide(
        &self,
        principal: &Principal,
        action: Action,
        board: Option<&str>,
        channel: Option<&str>,
    ) -> Decision;
}

/// The M0 policy: the local principal holds `all`. Anyone else holds nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalPolicy;

impl Policy for LocalPolicy {
    fn decide(
        &self,
        principal: &Principal,
        _action: Action,
        _board: Option<&str>,
        _channel: Option<&str>,
    ) -> Decision {
        if principal.kind == ActorKind::Local {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }
}

/// The single enforcement point. Cheap to clone.
#[derive(Clone)]
pub struct Authorizer {
    policy: Arc<dyn Policy>,
}

impl Authorizer {
    pub fn new(policy: Arc<dyn Policy>) -> Self {
        Self { policy }
    }

    /// Decides whether `principal` may perform `action` on `board` and,
    /// where one is given, on `channel`.
    pub fn authorize(
        &self,
        principal: &Principal,
        action: Action,
        board: Option<&str>,
        channel: Option<&str>,
    ) -> Decision {
        self.policy.decide(principal, action, board, channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIONS: [Action; 5] = [
        Action::Health,
        Action::Read,
        Action::Post,
        Action::CreateChannel,
        Action::CreateBoard,
    ];

    #[test]
    fn local_principal_holds_all() {
        let authz = Authorizer::new(Arc::new(LocalPolicy));
        for action in ACTIONS {
            let decision = authz.authorize(&Principal::local(), action, Some("general"), Some("a"));
            assert_eq!(decision, Decision::Allow, "{action:?}");
        }
    }

    #[test]
    fn other_principals_hold_nothing() {
        let authz = Authorizer::new(Arc::new(LocalPolicy));
        let stranger = Principal {
            id: "svc_1".into(),
            kind: ActorKind::Service,
            handle: "ci".into(),
        };
        for action in ACTIONS {
            let decision = authz.authorize(&stranger, action, Some("general"), None);
            assert_eq!(decision, Decision::Deny, "{action:?}");
        }
    }

    #[test]
    fn actor_ids_are_stable_per_principal_and_profile() {
        let local = Principal::local();
        assert_eq!(local.actor(None), local.actor(None));
        assert_eq!(local.actor(Some("fx")), local.actor(Some("fx")));
        assert_ne!(local.actor(None).id, local.actor(Some("fx")).id);
        assert_ne!(local.actor(Some("fx")).id, local.actor(Some("fy")).id);
        assert_eq!(local.actor(None).handle, "local");
        assert_eq!(local.actor(Some("fx")).handle, "local/fx");
        assert_eq!(local.actor(Some("fx")).profile.as_deref(), Some("fx"));
    }
}
