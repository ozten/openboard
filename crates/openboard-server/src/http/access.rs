//! What a caller on a network board may do: the grants and ceilings in
//! force when its request was authenticated, evaluated by
//! [`crate::grants`]. `docs/permissions.md` is the specification.

use openboard_api::ActorKind;

use crate::authz::Action;
use crate::grants::{self, Ceiling, Subject, Verb, Verbs};
use crate::store::{Access, Snapshot};
use crate::time::now_ms;

/// A caller's authority on a network board.
#[derive(Debug, Clone)]
pub(crate) struct Authority {
    pub snapshot: Snapshot,
    pub subject: Subject,
    /// The profile's narrowing and the scopes of the credential and every
    /// credential it was derived from, by board.
    pub ceilings: Vec<(String, Ceiling)>,
    /// The limits on the declared profile (A8), by board. Unlike a token's
    /// scope, a profile limited on one board is not limited on another.
    pub profile_limits: Vec<(String, Ceiling)>,
    /// The request came with a derived token.
    pub derived: bool,
}

impl Authority {
    pub(crate) fn new(snapshot: Snapshot, subject: Subject) -> Self {
        Self {
            snapshot,
            subject,
            ceilings: Vec::new(),
            profile_limits: Vec::new(),
            derived: false,
        }
    }

    /// The ceilings that apply on `board`. A ceiling for another board is a
    /// ceiling of nothing here.
    fn ceilings_on(&self, board: &str) -> Vec<&Ceiling> {
        static NOTHING: Ceiling = Ceiling {
            entries: Vec::new(),
        };
        let key = crate::names::board_key(board);
        self.ceilings
            .iter()
            .map(|(on, ceiling)| {
                if crate::names::board_key(on) == key {
                    ceiling
                } else {
                    &NOTHING
                }
            })
            .chain(
                self.profile_limits
                    .iter()
                    .filter(|(on, _)| crate::names::board_key(on) == key)
                    .map(|(_, ceiling)| ceiling),
            )
            .collect()
    }

    /// The ceilings that apply on `board`, owned.
    pub(crate) fn ceilings_for(&self, board: &str) -> Vec<Ceiling> {
        self.ceilings_on(board).into_iter().cloned().collect()
    }

    /// The grants of `board`. A board that does not exist yet is judged by
    /// the grants it would start with if this caller created it: `all` for
    /// its creator. Creating it is a separate check ([`Action::CreateBoard`]).
    fn grants_of(&self, board: &str) -> std::borrow::Cow<'_, [grants::Grant]> {
        if self.snapshot.has_board(board) {
            std::borrow::Cow::Borrowed(self.snapshot.grants(board))
        } else {
            std::borrow::Cow::Owned(vec![grants::Grant {
                id: String::new(),
                pattern: grants::Pattern::Board,
                grantee: grants::Grantee::Principal(self.subject.id.clone()),
                verbs: Verbs::all(),
                expires_at: None,
            }])
        }
    }

    /// The caller's access to one channel of `board`.
    pub(crate) fn access(&self, board: &str, channel: &str) -> grants::Access {
        grants::effective(
            &self.grants_of(board),
            &self.subject,
            &self.ceilings_on(board),
            channel,
            now_ms(),
        )
    }

    pub(crate) fn allows(&self, action: Action, board: &str, channel: Option<&str>) -> bool {
        match (action, channel) {
            (Action::CreateBoard, _) => {
                self.snapshot.access == Access::Open
                    && matches!(self.subject.kind, ActorKind::Agent | ActorKind::Person)
                    && !self.derived
            }
            (Action::Read, None) => grants::holds_somewhere(
                &self.grants_of(board),
                &self.subject,
                &self.ceilings_on(board),
                Verb::Read,
                now_ms(),
            ),
            // Across a subtree: whether the caller holds the verbs anywhere.
            // Each item found is then judged in its own channel.
            (action, None) => needed(action).is_some_and(|verbs| {
                verbs.iter().all(|verb| {
                    grants::holds_somewhere(
                        &self.grants_of(board),
                        &self.subject,
                        &self.ceilings_on(board),
                        verb,
                        now_ms(),
                    )
                })
            }),
            (action, Some(channel)) => match needed(action) {
                Some(verbs) => self.access(board, channel).verbs.contains_all(verbs),
                None => false,
            },
        }
    }
}

/// The verbs an action needs in its channel. Every action on a channel
/// needs `read` too: a caller that cannot read a channel is told it does not
/// exist, whatever it tries there.
fn needed(action: Action) -> Option<Verbs> {
    let verb = match action {
        Action::Read => Verb::Read,
        Action::Post | Action::Edit | Action::Delete => Verb::Post,
        Action::CreateChannel => Verb::Create,
        Action::Label => Verb::Label,
        Action::React => Verb::React,
        Action::CreateTask => Verb::TaskCreate,
        Action::ClaimTask => Verb::TaskClaim,
        Action::ManageTasks => Verb::TaskManage,
        Action::Health
        | Action::CreateBoard
        | Action::Enrol
        | Action::Session
        | Action::SignIn
        | Action::Page => return None,
    };
    Some(Verbs::of(&[Verb::Read, verb]))
}
