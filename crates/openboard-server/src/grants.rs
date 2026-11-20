//! The permission algebra of milestone M4: verbs, patterns, grants, the
//! evaluation rule, ceilings and the authority to change grants.
//!
//! Pure: no database and no clock. `docs/permissions.md` is the
//! specification, and the tests below are its examples.

use std::fmt;

use openboard_api::ActorKind;

/// One permission verb (design document, section 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Verb {
    Read,
    Post,
    React,
    Label,
    Attach,
    Create,
    TaskCreate,
    TaskClaim,
    TaskManage,
    Approve,
    SyncImport,
    SyncExport,
    GrantManage,
    IntegrateManage,
    AuditRead,
    Export,
    Delete,
}

impl Verb {
    pub const ALL: [Verb; 17] = [
        Verb::Read,
        Verb::Post,
        Verb::React,
        Verb::Label,
        Verb::Attach,
        Verb::Create,
        Verb::TaskCreate,
        Verb::TaskClaim,
        Verb::TaskManage,
        Verb::Approve,
        Verb::SyncImport,
        Verb::SyncExport,
        Verb::GrantManage,
        Verb::IntegrateManage,
        Verb::AuditRead,
        Verb::Export,
        Verb::Delete,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Post => "post",
            Verb::React => "react",
            Verb::Label => "label",
            Verb::Attach => "attach",
            Verb::Create => "create",
            Verb::TaskCreate => "task.create",
            Verb::TaskClaim => "task.claim",
            Verb::TaskManage => "task.manage",
            Verb::Approve => "approve",
            Verb::SyncImport => "sync.import",
            Verb::SyncExport => "sync.export",
            Verb::GrantManage => "grant.manage",
            Verb::IntegrateManage => "integrate.manage",
            Verb::AuditRead => "audit.read",
            Verb::Export => "export",
            Verb::Delete => "delete",
        }
    }

    fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// A set of verbs.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Verbs(u32);

impl Verbs {
    pub const NONE: Verbs = Verbs(0);

    pub fn all() -> Self {
        Verb::ALL
            .iter()
            .fold(Verbs::NONE, |set, verb| set.with(*verb))
    }

    pub fn of(verbs: &[Verb]) -> Self {
        verbs.iter().fold(Verbs::NONE, |set, verb| set.with(*verb))
    }

    pub fn with(self, verb: Verb) -> Self {
        Verbs(self.0 | verb.bit())
    }

    pub fn contains(self, verb: Verb) -> bool {
        self.0 & verb.bit() != 0
    }

    pub fn contains_all(self, other: Verbs) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn union(self, other: Verbs) -> Self {
        Verbs(self.0 | other.0)
    }

    pub fn intersect(self, other: Verbs) -> Self {
        Verbs(self.0 & other.0)
    }

    /// The verbs in `self` that are not in `other`.
    pub fn minus(self, other: Verbs) -> Self {
        Verbs(self.0 & !other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Verb> {
        Verb::ALL
            .into_iter()
            .filter(move |verb| self.contains(*verb))
    }

    /// Parses `read,post,task`: verbs and the shorthands `task`, `sync`,
    /// `manage`, `all` and `none`, separated by commas.
    pub fn parse(raw: &str) -> Result<Self, GrantError> {
        let mut set = Verbs::NONE;
        let mut none = false;
        let mut any = false;
        for word in raw.split(',').map(str::trim) {
            any = true;
            let add = match word {
                "none" => {
                    none = true;
                    Verbs::NONE
                }
                "all" => Verbs::all(),
                "task" => Verbs::of(&[Verb::TaskCreate, Verb::TaskClaim]),
                "sync" => Verbs::of(&[Verb::SyncImport, Verb::SyncExport]),
                "manage" => Verbs::of(&[
                    Verb::GrantManage,
                    Verb::IntegrateManage,
                    Verb::AuditRead,
                    Verb::Export,
                    Verb::Delete,
                ]),
                _ => match Verb::ALL.iter().find(|verb| verb.name() == word) {
                    Some(verb) => Verbs::NONE.with(*verb),
                    None => return Err(GrantError::UnknownVerb(word.to_owned())),
                },
            };
            set = set.union(add);
        }
        if !any || raw.trim().is_empty() {
            return Err(GrantError::UnknownVerb(String::new()));
        }
        if none && !set.is_empty() {
            return Err(GrantError::NoneWithOthers);
        }
        Ok(set)
    }
}

impl fmt::Display for Verbs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        if *self == Verbs::all() {
            return f.write_str("all");
        }
        let names: Vec<&str> = self.iter().map(Verb::name).collect();
        f.write_str(&names.join(","))
    }
}

impl fmt::Debug for Verbs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Verbs({self})")
    }
}

/// Where a grant or a ceiling entry applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Pattern {
    /// `*`: the whole board.
    Board,
    /// `build/`: the channel `build` and every channel under it. Held
    /// without the trailing slash.
    Subtree(String),
    /// `build/api`: that one channel.
    Channel(String),
}

impl Pattern {
    /// Parses `*`, `build/` or `build/api`. `validate` checks a channel path.
    pub fn parse(
        raw: &str,
        validate: impl Fn(&str) -> Result<(), &'static str>,
    ) -> Result<Self, GrantError> {
        if raw == "*" {
            return Ok(Pattern::Board);
        }
        let (path, subtree) = match raw.strip_suffix('/') {
            Some(path) => (path, true),
            None => (raw, false),
        };
        validate(path).map_err(|why| GrantError::BadPattern(why.to_owned()))?;
        Ok(if subtree {
            Pattern::Subtree(path.to_owned())
        } else {
            Pattern::Channel(path.to_owned())
        })
    }

    pub fn matches(&self, channel: &str) -> bool {
        match self {
            Pattern::Board => true,
            Pattern::Channel(path) => channel == path,
            Pattern::Subtree(path) => within(channel, path),
        }
    }

    /// True when every channel `other` matches, `self` matches too.
    pub fn contains(&self, other: &Pattern) -> bool {
        match (self, other) {
            (Pattern::Board, _) => true,
            (_, Pattern::Board) => false,
            (Pattern::Subtree(path), Pattern::Subtree(inner) | Pattern::Channel(inner)) => {
                within(inner, path)
            }
            (Pattern::Channel(path), Pattern::Channel(inner)) => path == inner,
            (Pattern::Channel(_), Pattern::Subtree(_)) => false,
        }
    }

    /// Larger is more specific. Two patterns that match the same channel are
    /// ordered by this alone: an exact channel beats any subtree, and of two
    /// subtrees of one channel, the longer is inside the shorter.
    fn specificity(&self) -> (u8, usize) {
        match self {
            Pattern::Board => (0, 0),
            Pattern::Subtree(path) => (1, path.len()),
            Pattern::Channel(_) => (2, 0),
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pattern::Board => f.write_str("*"),
            Pattern::Subtree(path) => write!(f, "{path}/"),
            Pattern::Channel(path) => f.write_str(path),
        }
    }
}

/// `channel` is `path` or under it. `build` is not under `buil`.
fn within(channel: &str, path: &str) -> bool {
    channel == path
        || channel
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A group of principals, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Agents,
    People,
    Services,
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::Agents => "agents",
            Group::People => "people",
            Group::Services => "services",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "agents" => Some(Group::Agents),
            "people" => Some(Group::People),
            "services" => Some(Group::Services),
            _ => None,
        }
    }

    fn has(self, kind: ActorKind) -> bool {
        matches!(
            (self, kind),
            (Group::Agents, ActorKind::Agent)
                | (Group::People, ActorKind::Person)
                | (Group::Services, ActorKind::Service)
        )
    }
}

/// Who a grant is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Grantee {
    /// One principal, by ID.
    Principal(String),
    Group(Group),
    /// Every principal. Only ever given `none`.
    Everyone,
}

impl Grantee {
    /// Larger is closer: a named principal, then a group, then everyone.
    fn closeness(&self) -> u8 {
        match self {
            Grantee::Principal(_) => 2,
            Grantee::Group(_) => 1,
            Grantee::Everyone => 0,
        }
    }

    fn applies_to(&self, subject: &Subject) -> bool {
        match self {
            Grantee::Principal(id) => *id == subject.id,
            Grantee::Group(group) => group.has(subject.kind),
            Grantee::Everyone => true,
        }
    }
}

/// A principal whose access is being decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub id: String,
    pub kind: ActorKind,
}

/// One grant on one board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub pattern: Pattern,
    pub grantee: Grantee,
    pub verbs: Verbs,
    /// Milliseconds since the Unix epoch. `None` never expires.
    pub expires_at: Option<i64>,
}

impl Grant {
    pub fn is_active(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|end| now < end)
    }

    /// The rules every grant obeys whoever makes it.
    pub fn validate(&self) -> Result<(), GrantError> {
        if self.grantee == Grantee::Everyone {
            if !self.verbs.is_empty() {
                return Err(GrantError::EveryoneOnlyNone);
            }
            if self.expires_at.is_some() {
                return Err(GrantError::EveryoneNeverExpires);
            }
        }
        Ok(())
    }

    fn key(&self) -> (u8, usize, u8) {
        let (class, length) = self.pattern.specificity();
        (class, length, self.grantee.closeness())
    }
}

/// What a caller may do in one channel, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub verbs: Verbs,
    /// The IDs of the grants that decided, in the order given. Empty means
    /// no grant applies: no access.
    pub deciding: Vec<String>,
    /// A ceiling removed at least one verb the grants gave.
    pub narrowed: bool,
}

/// The deciding grants for `subject` at `channel`: the active, applicable
/// grants with the most specific pattern and, at it, the closest grantee.
fn deciders<'a>(
    grants: &'a [Grant],
    applies: impl Fn(&Grant) -> bool,
    channel: &str,
    now: i64,
) -> Vec<&'a Grant> {
    let candidates: Vec<&Grant> = grants
        .iter()
        .filter(|grant| grant.is_active(now) && grant.pattern.matches(channel) && applies(grant))
        .collect();
    let Some(best) = candidates.iter().map(|grant| grant.key()).max() else {
        return Vec::new();
    };
    candidates
        .into_iter()
        .filter(|grant| grant.key() == best)
        .collect()
}

/// The access `subject` holds in `channel` by grants alone.
pub fn evaluate(grants: &[Grant], subject: &Subject, channel: &str, now: i64) -> Access {
    let deciding = deciders(
        grants,
        |grant| grant.grantee.applies_to(subject),
        channel,
        now,
    );
    Access {
        verbs: deciding
            .iter()
            .fold(Verbs::NONE, |set, grant| set.union(grant.verbs)),
        deciding: deciding.iter().map(|grant| grant.id.clone()).collect(),
        narrowed: false,
    }
}

/// A limit on a request: a profile's narrowing or a derived token's scope.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ceiling {
    pub entries: Vec<(Pattern, Verbs)>,
}

impl Ceiling {
    /// Parses entries such as `build/=read,post,label,task`.
    pub fn parse_entry(
        raw: &str,
        validate: impl Fn(&str) -> Result<(), &'static str>,
    ) -> Result<(Pattern, Verbs), GrantError> {
        let (pattern, verbs) = raw
            .split_once('=')
            .ok_or_else(|| GrantError::BadPattern("expected pattern=verbs".to_owned()))?;
        Ok((Pattern::parse(pattern, validate)?, Verbs::parse(verbs)?))
    }

    /// The verbs this ceiling allows in `channel`: the most specific
    /// matching entries, joined. Nothing where no entry matches.
    pub fn allows(&self, channel: &str) -> Verbs {
        let matching = self
            .entries
            .iter()
            .filter(|(pattern, _)| pattern.matches(channel));
        let Some(best) = matching
            .clone()
            .map(|(pattern, _)| pattern.specificity())
            .max()
        else {
            return Verbs::NONE;
        };
        matching
            .filter(|(pattern, _)| pattern.specificity() == best)
            .fold(Verbs::NONE, |set, (_, verbs)| set.union(*verbs))
    }
}

/// The access of a request: its principal's grants, intersected with every
/// ceiling on the request.
pub fn effective(
    grants: &[Grant],
    subject: &Subject,
    ceilings: &[&Ceiling],
    channel: &str,
    now: i64,
) -> Access {
    let mut access = evaluate(grants, subject, channel, now);
    for ceiling in ceilings {
        let narrowed = access.verbs.intersect(ceiling.allows(channel));
        access.narrowed |= narrowed != access.verbs;
        access.verbs = narrowed;
    }
    access
}

/// Whether a request holds `verb` in at least one channel, existing or
/// not. Exact: access is the same everywhere in a region that no grant or
/// ceiling pattern divides, so one probe per pattern covers every region.
pub fn holds_somewhere(
    grants: &[Grant],
    subject: &Subject,
    ceilings: &[&Ceiling],
    verb: Verb,
    now: i64,
) -> bool {
    let patterns = grants
        .iter()
        .filter(|grant| grant.is_active(now))
        .map(|grant| &grant.pattern)
        .chain(
            ceilings
                .iter()
                .flat_map(|ceiling| ceiling.entries.iter().map(|(pattern, _)| pattern)),
        );
    std::iter::once(&Pattern::Board)
        .chain(patterns)
        .any(|pattern| {
            effective(grants, subject, ceilings, &probe(pattern), now)
                .verbs
                .contains(verb)
        })
}

/// A request that wants to change grants.
#[derive(Debug, Clone, Copy)]
pub struct Issuer<'a> {
    pub subject: &'a Subject,
    /// The profile's narrowing, if any. A derived token never gets here.
    pub ceilings: &'a [&'a Ceiling],
    /// The request came with a derived token.
    pub derived: bool,
}

/// The grant held by `issuer` that gives authority at `pattern` over
/// `verbs`, preferring one that never expires, then the latest expiry.
///
/// Authority is not shadowed by narrower grants: any active grant to the
/// issuer (named or through a group, never `everyone`) whose pattern contains
/// `pattern` and which includes `grant.manage` and every verb in `verbs`.
pub fn authority<'a>(
    grants: &'a [Grant],
    issuer: &Issuer<'_>,
    pattern: &Pattern,
    verbs: Verbs,
    now: i64,
) -> Result<&'a Grant, GrantError> {
    if issuer.derived {
        return Err(GrantError::DerivedToken);
    }
    let probe = probe(pattern);
    let mut ceiling = Verbs::all();
    for limit in issuer.ceilings {
        ceiling = ceiling.intersect(limit.allows(&probe));
    }
    let needed = verbs.with(Verb::GrantManage);
    if !ceiling.contains_all(needed) {
        return Err(GrantError::NoAuthority {
            pattern: pattern.clone(),
            missing: needed.minus(ceiling),
        });
    }
    let held = grants.iter().filter(|grant| {
        grant.is_active(now)
            && grant.grantee != Grantee::Everyone
            && grant.grantee.applies_to(issuer.subject)
            && grant.pattern.contains(pattern)
            && grant.verbs.contains(Verb::GrantManage)
    });
    let mut best: Option<&Grant> = None;
    let mut widest = Verbs::NONE;
    for grant in held {
        widest = widest.union(grant.verbs);
        if !grant.verbs.contains_all(needed) {
            continue;
        }
        let later = match (best.map(|b| b.expires_at), grant.expires_at) {
            (None, _) => true,
            (Some(None), _) => false,
            (Some(Some(_)), None) => true,
            (Some(Some(old)), Some(new)) => new > old,
        };
        if later {
            best = Some(grant);
        }
    }
    best.ok_or(GrantError::NoAuthority {
        pattern: pattern.clone(),
        missing: needed.minus(if widest.contains(Verb::GrantManage) {
            widest
        } else {
            Verbs::NONE
        }),
    })
}

/// Checks that `issuer` may add `grant`. Returns the grant that authorizes
/// it.
pub fn check_add<'a>(
    grants: &'a [Grant],
    issuer: &Issuer<'_>,
    grant: &Grant,
    now: i64,
) -> Result<&'a Grant, GrantError> {
    grant.validate()?;
    let by = authority(grants, issuer, &grant.pattern, grant.verbs, now)?;
    if let Some(limit) = by.expires_at {
        match grant.expires_at {
            Some(end) if end <= limit => {}
            _ => return Err(GrantError::OutlivesAuthority { limit }),
        }
    }
    Ok(by)
}

/// Checks that `issuer` may remove the grant `id`. Returns the grant that
/// authorizes it.
pub fn check_remove<'a>(
    grants: &'a [Grant],
    issuer: &Issuer<'_>,
    id: &str,
    now: i64,
) -> Result<&'a Grant, GrantError> {
    let target = grants
        .iter()
        .find(|grant| grant.id == id)
        .ok_or(GrantError::NoSuchGrant)?;
    authority(grants, issuer, &target.pattern, Verbs::NONE, now)
}

/// The grants that make `path` a private channel owned by `creator`, or
/// why it cannot be one. The caller checks that no channel at or under
/// `path` exists, and writes these with the channel in one transaction.
pub fn private_channel(
    grants: &[Grant],
    creator: &Issuer<'_>,
    path: &str,
    now: i64,
    ids: (String, String),
) -> Result<[Grant; 2], GrantError> {
    if creator.derived {
        return Err(GrantError::DerivedToken);
    }
    let access = effective(grants, creator.subject, creator.ceilings, path, now);
    if !access.verbs.contains(Verb::Create) {
        return Err(GrantError::NoAuthority {
            pattern: Pattern::Channel(path.to_owned()),
            missing: Verbs::NONE.with(Verb::Create),
        });
    }
    let closed = Pattern::Subtree(path.to_owned());
    if grants
        .iter()
        .any(|grant| grant.is_active(now) && closed.contains(&grant.pattern))
    {
        return Err(GrantError::AlreadyReached);
    }
    Ok([
        Grant {
            id: ids.0,
            pattern: closed.clone(),
            grantee: Grantee::Everyone,
            verbs: Verbs::NONE,
            expires_at: None,
        },
        Grant {
            id: ids.1,
            pattern: closed,
            grantee: Grantee::Principal(creator.subject.id.clone()),
            verbs: access.verbs.with(Verb::GrantManage),
            expires_at: None,
        },
    ])
}

/// The patterns at which adding `grant` gives `subject` `read` where it had
/// none: one per region of the board, each named by its pattern. An
/// administrator's grant to themselves announces itself there (Z9).
pub fn opens_reading(grants: &[Grant], subject: &Subject, grant: &Grant, now: i64) -> Vec<Pattern> {
    let mut after = grants.to_vec();
    after.push(grant.clone());
    let mut regions = vec![grant.pattern.clone()];
    for other in grants {
        if other.is_active(now)
            && grant.pattern.contains(&other.pattern)
            && !regions.contains(&other.pattern)
        {
            regions.push(other.pattern.clone());
        }
    }
    regions
        .into_iter()
        .filter(|region| {
            let probe = probe(region);
            !evaluate(grants, subject, &probe, now)
                .verbs
                .contains(Verb::Read)
                && evaluate(&after, subject, &probe, now)
                    .verbs
                    .contains(Verb::Read)
        })
        .collect()
}

/// A channel name standing for a channel in `pattern`'s region that no
/// narrower pattern matches. It contains a NUL, which no real channel does.
fn probe(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Board => "\0".to_owned(),
        Pattern::Subtree(path) => format!("{path}/\0"),
        Pattern::Channel(path) => path.clone(),
    }
}

/// Why a grant, a ceiling or a change of grants was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    UnknownVerb(String),
    NoneWithOthers,
    BadPattern(String),
    EveryoneOnlyNone,
    EveryoneNeverExpires,
    DerivedToken,
    NoAuthority { pattern: Pattern, missing: Verbs },
    OutlivesAuthority { limit: i64 },
    NoSuchGrant,
    AlreadyReached,
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrantError::UnknownVerb(word) if word.is_empty() => {
                f.write_str("no verbs given; use none to give nothing")
            }
            GrantError::UnknownVerb(word) => write!(f, "unknown verb {word:?}"),
            GrantError::NoneWithOthers => f.write_str("none cannot be combined with other verbs"),
            GrantError::BadPattern(why) => write!(f, "invalid pattern: {why}"),
            GrantError::EveryoneOnlyNone => {
                f.write_str("a grant to everyone can only be none; grant to a group to open access")
            }
            GrantError::EveryoneNeverExpires => {
                f.write_str("a grant to everyone closes channels and cannot expire")
            }
            GrantError::DerivedToken => {
                f.write_str("a derived token cannot change grants or create private channels")
            }
            GrantError::NoAuthority { pattern, missing } if missing.is_empty() => {
                write!(f, "no grant gives you grant.manage at {pattern}")
            }
            GrantError::NoAuthority { pattern, missing } => {
                write!(f, "you do not hold {missing} at {pattern}")
            }
            GrantError::OutlivesAuthority { limit } => write!(
                f,
                "the grant that authorizes this expires; the new grant must expire by then ({limit})"
            ),
            GrantError::NoSuchGrant => f.write_str("no such grant"),
            GrantError::AlreadyReached => f.write_str(
                "a grant already applies at or under this path; a private channel must start with none",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(_: &str) -> Result<(), &'static str> {
        Ok(())
    }

    fn pattern(raw: &str) -> Pattern {
        Pattern::parse(raw, ok).unwrap()
    }

    fn verbs(raw: &str) -> Verbs {
        Verbs::parse(raw).unwrap()
    }

    fn grantee(raw: &str) -> Grantee {
        match raw {
            "everyone" => Grantee::Everyone,
            other => Group::parse(other)
                .map(Grantee::Group)
                .unwrap_or_else(|| Grantee::Principal(other.to_owned())),
        }
    }

    /// Grants from lines of `pattern grantee verbs [expires]`.
    fn table(text: &str) -> Vec<Grant> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .enumerate()
            .map(|(n, line)| {
                let words: Vec<&str> = line.split_whitespace().collect();
                Grant {
                    id: format!("g{n}"),
                    pattern: pattern(words[0]),
                    grantee: grantee(words[1]),
                    verbs: verbs(words[2]),
                    expires_at: words.get(3).map(|end| end.parse().unwrap()),
                }
            })
            .collect()
    }

    fn agent(id: &str) -> Subject {
        Subject {
            id: id.to_owned(),
            kind: ActorKind::Agent,
        }
    }

    fn person(id: &str) -> Subject {
        Subject {
            id: id.to_owned(),
            kind: ActorKind::Person,
        }
    }

    fn access(grants: &[Grant], subject: &Subject, channel: &str) -> String {
        evaluate(grants, subject, channel, 0).verbs.to_string()
    }

    fn issuer(subject: &Subject) -> Issuer<'_> {
        Issuer {
            subject,
            ceilings: &[],
            derived: false,
        }
    }

    const SPEC: &str = "
        *        agents     read,post
        build/   worker-1   read,post,task.claim
        hr/      everyone   none
        hr/      hr-bot     read
    ";

    #[test]
    fn the_spec_example() {
        let grants = table(SPEC);
        assert_eq!(
            access(&grants, &agent("worker-1"), "build/api"),
            "read,post,task.claim"
        );
        assert_eq!(
            access(&grants, &agent("worker-2"), "build/api"),
            "read,post"
        );
        assert_eq!(access(&grants, &agent("worker-1"), "hr/payroll"), "none");
        assert_eq!(access(&grants, &agent("hr-bot"), "hr/payroll"), "read");
        assert_eq!(access(&grants, &agent("hr-bot"), "hr"), "read");
        assert_eq!(access(&grants, &person("ada"), "build/api"), "none");
    }

    #[test]
    fn the_winning_set_replaces_broader_ones() {
        let grants = table(
            "
            *        worker-1   read,post
            build/   worker-1   read
            ",
        );
        assert_eq!(access(&grants, &agent("worker-1"), "build/api"), "read");
        assert_eq!(access(&grants, &agent("worker-1"), "docs"), "read,post");
    }

    #[test]
    fn specificity_orders_exact_then_longest_subtree_then_board() {
        let grants = table(
            "
            *            a   read
            build/       a   read,post
            build/api/   a   read,react
            build/api    a   read,label
            ",
        );
        let a = agent("a");
        assert_eq!(access(&grants, &a, "build/api"), "read,label");
        assert_eq!(access(&grants, &a, "build/api/v2"), "read,react");
        assert_eq!(access(&grants, &a, "build/web"), "read,post");
        assert_eq!(access(&grants, &a, "build"), "read,post");
        assert_eq!(access(&grants, &a, "docs"), "read");
    }

    #[test]
    fn a_subtree_is_not_a_string_prefix() {
        let grants = table("build/ a read");
        assert_eq!(access(&grants, &agent("a"), "builder"), "none");
        assert_eq!(access(&grants, &agent("a"), "build"), "read");
        assert!(!pattern("build/").contains(&pattern("builder/x")));
        assert!(!pattern("build/").contains(&pattern("builder")));
    }

    #[test]
    fn closeness_breaks_ties_at_one_pattern() {
        let grants = table(
            "
            build/   everyone   none
            build/   agents     read
            build/   a          read,post
            ",
        );
        assert_eq!(access(&grants, &agent("a"), "build/x"), "read,post");
        assert_eq!(access(&grants, &agent("b"), "build/x"), "read");
        assert_eq!(access(&grants, &person("p"), "build/x"), "none");
    }

    #[test]
    fn pattern_beats_closeness() {
        let grants = table(
            "
            *        a          all
            build/   everyone   none
            ",
        );
        assert_eq!(access(&grants, &agent("a"), "build/x"), "none");
    }

    #[test]
    fn ties_add_up_and_none_adds_nothing() {
        let grants = table(
            "
            build/   agents   none
            build/   agents   read
            build/   a        post
            build/   a        react
            ",
        );
        assert_eq!(access(&grants, &agent("a"), "build/x"), "post,react");
        assert_eq!(access(&grants, &agent("b"), "build/x"), "read");
    }

    #[test]
    fn no_grant_means_no_access() {
        let access = evaluate(&[], &agent("a"), "x", 0);
        assert!(access.verbs.is_empty());
        assert!(access.deciding.is_empty());
    }

    #[test]
    fn an_expired_grant_stops_applying_and_a_broader_one_decides() {
        let grants = table(
            "
            *        a   read
            build/   a   read,post   100
            ",
        );
        let a = agent("a");
        assert_eq!(
            evaluate(&grants, &a, "build/x", 99).verbs,
            verbs("read,post")
        );
        assert_eq!(evaluate(&grants, &a, "build/x", 100).verbs, verbs("read"));
        assert_eq!(evaluate(&grants, &a, "build/x", 100).deciding, ["g0"]);
    }

    #[test]
    fn shorthands_and_their_limits() {
        assert_eq!(verbs("task").to_string(), "task.create,task.claim");
        assert_eq!(verbs("all"), Verbs::all());
        assert_eq!(verbs("none"), Verbs::NONE);
        let manage = verbs("manage");
        assert!(!manage.contains(Verb::TaskManage));
        assert!(!manage.contains(Verb::Approve));
        assert!(manage.contains(Verb::GrantManage));
        assert_eq!(Verbs::parse("none,read"), Err(GrantError::NoneWithOthers));
        assert!(Verbs::parse("rede").is_err());
        assert!(Verbs::parse("").is_err());
        assert_eq!(verbs(&Verbs::all().to_string()), Verbs::all());
    }

    #[test]
    fn everyone_is_only_ever_closed_and_forever() {
        let mut grant = table("hr/ everyone none")[0].clone();
        assert_eq!(grant.validate(), Ok(()));
        grant.verbs = verbs("read");
        assert_eq!(grant.validate(), Err(GrantError::EveryoneOnlyNone));
        grant.verbs = Verbs::NONE;
        grant.expires_at = Some(5);
        assert_eq!(grant.validate(), Err(GrantError::EveryoneNeverExpires));
    }

    #[test]
    fn ceilings_narrow_and_never_widen() {
        let grants = table("* worker all");
        let profile = Ceiling {
            entries: vec![(pattern("build/"), verbs("read,post,label,task.claim"))],
        };
        let token = Ceiling {
            entries: vec![
                (pattern("build/"), verbs("read,post")),
                (pattern("build/secret"), verbs("none")),
            ],
        };
        let w = agent("worker");
        let at = |channel| effective(&grants, &w, &[&profile, &token], channel, 0);
        assert_eq!(at("build/api").verbs, verbs("read,post"));
        assert!(at("build/api").narrowed);
        assert_eq!(at("build/secret").verbs, Verbs::NONE);
        assert_eq!(at("docs").verbs, Verbs::NONE);
        // A ceiling wider than the grants adds nothing.
        let narrow = table("build/ worker read");
        let wide = Ceiling {
            entries: vec![(pattern("*"), Verbs::all())],
        };
        assert_eq!(
            effective(&narrow, &w, &[&wide], "build/x", 0).verbs,
            verbs("read")
        );
        assert_eq!(
            effective(&narrow, &w, &[&wide], "docs", 0).verbs,
            Verbs::NONE
        );
    }

    #[test]
    fn granting_needs_authority_holding_every_verb() {
        let grants = table(
            "
            *        admin   all
            build/   lead    read,post,task,grant.manage
            build/   other   read,post
            ",
        );
        let lead = agent("lead");
        let new = |text: &str| table(text).remove(0);
        // Within its subtree and its verbs: allowed, under its own grant.
        let by = check_add(
            &grants,
            &issuer(&lead),
            &new("build/api w read,task.claim"),
            0,
        )
        .unwrap();
        assert_eq!(by.id, "g1");
        // More than it holds.
        assert_eq!(
            check_add(&grants, &issuer(&lead), &new("build/ w read,delete"), 0),
            Err(GrantError::NoAuthority {
                pattern: pattern("build/"),
                missing: verbs("delete"),
            })
        );
        // Outside its subtree.
        assert!(check_add(&grants, &issuer(&lead), &new("docs/ w read"), 0).is_err());
        assert!(check_add(&grants, &issuer(&lead), &new("* w read"), 0).is_err());
        // No grant.manage at all.
        assert!(check_add(&grants, &issuer(&agent("other")), &new("build/x w read"), 0).is_err());
    }

    #[test]
    fn authority_is_not_shadowed_by_narrower_grants() {
        let grants = table(
            "
            *     admin      all
            hr/   everyone   none
            hr/   hr-bot     read
            ",
        );
        let admin = person("admin");
        assert_eq!(access(&grants, &admin, "hr/payroll"), "none");
        let me = table("hr/ admin read").remove(0);
        assert!(check_add(&grants, &issuer(&admin), &me, 0).is_ok());
        assert_eq!(opens_reading(&grants, &admin, &me, 0), [pattern("hr/")]);
        // A grant to someone else opens nothing for the admin.
        let them = table("hr/ hr-bot-2 read").remove(0);
        assert!(opens_reading(&grants, &admin, &them, 0).is_empty());
    }

    #[test]
    fn a_grant_cannot_outlive_its_authority() {
        let grants = table(
            "
            build/   lead   read,grant.manage   1000
            ",
        );
        let lead = agent("lead");
        let mut new = table("build/ w read").remove(0);
        assert_eq!(
            check_add(&grants, &issuer(&lead), &new, 0),
            Err(GrantError::OutlivesAuthority { limit: 1000 })
        );
        new.expires_at = Some(1000);
        assert!(check_add(&grants, &issuer(&lead), &new, 0).is_ok());
        // Once the authority has expired, there is none.
        assert!(check_add(&grants, &issuer(&lead), &new, 1000).is_err());
    }

    #[test]
    fn the_longest_lived_authority_is_used() {
        let grants = table(
            "
            build/   lead   read,grant.manage   1000
            *        lead   read,grant.manage
            ",
        );
        let new = table("build/x w read").remove(0);
        assert_eq!(
            check_add(&grants, &issuer(&agent("lead")), &new, 0)
                .unwrap()
                .id,
            "g1"
        );
    }

    #[test]
    fn derived_tokens_and_narrowed_profiles_cannot_change_grants() {
        let grants = table("* admin all");
        let admin = agent("admin");
        let new = table("build/ w read").remove(0);
        let derived = Issuer {
            subject: &admin,
            ceilings: &[],
            derived: true,
        };
        assert_eq!(
            check_add(&grants, &derived, &new, 0),
            Err(GrantError::DerivedToken)
        );
        let profile = Ceiling {
            entries: vec![(pattern("*"), verbs("read,post"))],
        };
        let narrowed = Issuer {
            subject: &admin,
            ceilings: &[&profile],
            derived: false,
        };
        assert!(check_add(&grants, &narrowed, &new, 0).is_err());
    }

    #[test]
    fn everyone_grants_give_no_authority() {
        // Only none is allowed for everyone anyway; authority never comes
        // from a grant to everyone.
        let grants = table("hr/ everyone none");
        assert!(authority(
            &grants,
            &issuer(&agent("a")),
            &pattern("hr/"),
            Verbs::NONE,
            0
        )
        .is_err());
    }

    #[test]
    fn removing_needs_authority_at_the_pattern() {
        let grants = table(
            "
            build/   lead       read,grant.manage
            build/   everyone   none
            *        agents     read
            ",
        );
        let lead = agent("lead");
        assert!(check_remove(&grants, &issuer(&lead), "g1", 0).is_ok());
        assert!(check_remove(&grants, &issuer(&lead), "g2", 0).is_err());
        assert_eq!(
            check_remove(&grants, &issuer(&lead), "nope", 0),
            Err(GrantError::NoSuchGrant)
        );
    }

    #[test]
    fn a_private_channel_closes_and_admits_its_creator() {
        let grants = table(
            "
            *          agents   read,post,create
            ops/inner  b        read
            ",
        );
        let a = agent("a");
        let b = agent("b");
        let made =
            private_channel(&grants, &issuer(&a), "plans", 0, ("p0".into(), "p1".into())).unwrap();
        let mut after = grants.clone();
        after.extend(made.iter().cloned());
        assert_eq!(access(&after, &a, "plans"), "read,post,create,grant.manage");
        assert_eq!(
            access(&after, &a, "plans/q3"),
            "read,post,create,grant.manage"
        );
        assert_eq!(access(&after, &b, "plans"), "none");
        assert_eq!(access(&after, &b, "planning"), "read,post,create");
        // The owner can admit someone with up to its own verbs.
        let admit = table("plans/ b read,post").remove(0);
        assert!(check_add(&after, &issuer(&a), &admit, 0).is_ok());
        let too_much = table("plans/ b read,delete").remove(0);
        assert!(check_add(&after, &issuer(&a), &too_much, 0).is_err());
        // Not where a grant already reaches, and not without create.
        assert_eq!(
            private_channel(&grants, &issuer(&a), "ops", 0, ("x".into(), "y".into())),
            Err(GrantError::AlreadyReached)
        );
        assert!(private_channel(
            &grants,
            &issuer(&a),
            "ops/inner",
            0,
            ("x".into(), "y".into())
        )
        .is_err());
        assert!(private_channel(
            &grants,
            &issuer(&person("p")),
            "plans",
            0,
            ("x".into(), "y".into())
        )
        .is_err());
    }

    #[test]
    fn holding_a_verb_somewhere_sees_through_narrower_grants_and_ceilings() {
        let grants = table(
            "
            *        a          read
            hr/      everyone   none
            ops/x    b          read
            ",
        );
        assert!(holds_somewhere(&grants, &agent("a"), &[], Verb::Read, 0));
        assert!(holds_somewhere(&grants, &agent("b"), &[], Verb::Read, 0));
        assert!(!holds_somewhere(&grants, &agent("c"), &[], Verb::Read, 0));
        assert!(!holds_somewhere(&grants, &agent("a"), &[], Verb::Post, 0));
        let wide = table("* w all");
        let narrow = Ceiling {
            entries: vec![(pattern("build/"), verbs("read"))],
        };
        assert!(holds_somewhere(
            &wide,
            &agent("w"),
            &[&narrow],
            Verb::Read,
            0
        ));
        assert!(!holds_somewhere(
            &wide,
            &agent("w"),
            &[&narrow],
            Verb::Post,
            0
        ));
    }

    #[test]
    fn the_upgrade_grants_keep_todays_access() {
        let grants = table(
            "
            *   agents   read,post,react,label,create,task
            *   people   read,post,react,label,create,task
            ",
        );
        for subject in [agent("a"), person("p")] {
            let verbs = evaluate(&grants, &subject, "any/channel", 0).verbs;
            assert!(verbs.contains_all(Verbs::of(&[
                Verb::Read,
                Verb::Post,
                Verb::React,
                Verb::Label,
                Verb::Create,
                Verb::TaskCreate,
                Verb::TaskClaim
            ])));
            assert!(!verbs.contains(Verb::TaskManage));
            assert!(!verbs.contains(Verb::GrantManage));
        }
        let service = Subject {
            id: "s".into(),
            kind: ActorKind::Service,
        };
        assert_eq!(access(&grants, &service, "x"), "none");
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        const SEGMENTS: [&str; 4] = ["a", "ab", "b", "c"];

        fn path() -> impl Strategy<Value = String> {
            prop::collection::vec(prop::sample::select(SEGMENTS.to_vec()), 1..4)
                .prop_map(|parts| parts.join("/"))
        }

        fn any_pattern() -> impl Strategy<Value = Pattern> {
            prop_oneof![
                Just(Pattern::Board),
                path().prop_map(Pattern::Subtree),
                path().prop_map(Pattern::Channel),
            ]
        }

        fn any_verbs() -> impl Strategy<Value = Verbs> {
            (0u32..(1 << Verb::ALL.len())).prop_map(Verbs)
        }

        const IDS: [&str; 3] = ["p1", "p2", "p3"];

        fn any_grantee() -> impl Strategy<Value = Grantee> {
            prop_oneof![
                prop::sample::select(IDS.to_vec()).prop_map(|id| Grantee::Principal(id.to_owned())),
                Just(Grantee::Group(Group::Agents)),
                Just(Grantee::Group(Group::People)),
                Just(Grantee::Everyone),
            ]
        }

        fn any_grants() -> impl Strategy<Value = Vec<Grant>> {
            prop::collection::vec(
                (
                    any_pattern(),
                    any_grantee(),
                    any_verbs(),
                    prop::option::of(1i64..20),
                ),
                0..10,
            )
            .prop_map(|rows| {
                rows.into_iter()
                    .enumerate()
                    .map(|(n, (pattern, grantee, verbs, expires_at))| {
                        let everyone = grantee == Grantee::Everyone;
                        Grant {
                            id: format!("g{n}"),
                            pattern,
                            grantee,
                            verbs: if everyone { Verbs::NONE } else { verbs },
                            expires_at: if everyone { None } else { expires_at },
                        }
                    })
                    .collect()
            })
        }

        fn any_subject() -> impl Strategy<Value = Subject> {
            (
                prop::sample::select(IDS.to_vec()),
                prop::sample::select(vec![
                    ActorKind::Agent,
                    ActorKind::Person,
                    ActorKind::Service,
                ]),
            )
                .prop_map(|(id, kind)| Subject {
                    id: id.to_owned(),
                    kind,
                })
        }

        fn any_ceiling() -> impl Strategy<Value = Ceiling> {
            prop::collection::vec((any_pattern(), any_verbs()), 0..4)
                .prop_map(|entries| Ceiling { entries })
        }

        proptest! {
            #[test]
            fn ceilings_only_narrow(
                grants in any_grants(),
                subject in any_subject(),
                ceilings in prop::collection::vec(any_ceiling(), 0..3),
                channel in path(),
                now in 0i64..25,
            ) {
                let refs: Vec<&Ceiling> = ceilings.iter().collect();
                let plain = evaluate(&grants, &subject, &channel, now).verbs;
                let limited = effective(&grants, &subject, &refs, &channel, now).verbs;
                prop_assert!(plain.contains_all(limited));
                for ceiling in &ceilings {
                    prop_assert!(ceiling.allows(&channel).contains_all(limited));
                }
            }

            #[test]
            fn deciding_grants_apply_match_are_active_and_tie(
                grants in any_grants(),
                subject in any_subject(),
                channel in path(),
                now in 0i64..25,
            ) {
                let access = evaluate(&grants, &subject, &channel, now);
                let deciding: Vec<&Grant> = access
                    .deciding
                    .iter()
                    .map(|id| grants.iter().find(|g| &g.id == id).unwrap())
                    .collect();
                let mut joined = Verbs::NONE;
                for grant in &deciding {
                    prop_assert!(grant.is_active(now));
                    prop_assert!(grant.pattern.matches(&channel));
                    prop_assert!(grant.grantee.applies_to(&subject));
                    prop_assert_eq!(grant.key(), deciding[0].key());
                    joined = joined.union(grant.verbs);
                }
                prop_assert_eq!(joined, access.verbs);
                // Nothing applicable is more specific or closer.
                for grant in &grants {
                    if grant.is_active(now)
                        && grant.pattern.matches(&channel)
                        && grant.grantee.applies_to(&subject)
                    {
                        prop_assert!(grant.key() <= deciding[0].key());
                    }
                }
            }

            #[test]
            fn a_grant_to_one_principal_changes_no_one_else(
                grants in any_grants(),
                subject in any_subject(),
                pattern in any_pattern(),
                verbs in any_verbs(),
                channel in path(),
            ) {
                let other = if subject.id == "p1" { "p2" } else { "p1" };
                let mut after = grants.clone();
                after.push(Grant {
                    id: "new".into(),
                    pattern,
                    grantee: Grantee::Principal(other.into()),
                    verbs,
                    expires_at: None,
                });
                prop_assert_eq!(
                    evaluate(&grants, &subject, &channel, 0).verbs,
                    evaluate(&after, &subject, &channel, 0).verbs
                );
            }

            #[test]
            fn an_authorized_grant_gives_only_what_its_issuer_holds(
                grants in any_grants(),
                issuer_subject in any_subject(),
                new_pattern in any_pattern(),
                new_grantee in any_grantee(),
                new_verbs in any_verbs(),
                channel in path(),
                now in 0i64..25,
            ) {
                let new = Grant {
                    id: "new".into(),
                    pattern: new_pattern,
                    grantee: new_grantee,
                    verbs: new_verbs,
                    expires_at: None,
                };
                let issuer = Issuer { subject: &issuer_subject, ceilings: &[], derived: false };
                if let Ok(by) = check_add(&grants, &issuer, &new, now) {
                    // The authorizing grant is the issuer's, active, reaches
                    // the new pattern and holds every verb it gives.
                    prop_assert!(by.grantee.applies_to(&issuer_subject));
                    prop_assert!(by.grantee != Grantee::Everyone);
                    prop_assert!(by.is_active(now));
                    prop_assert!(by.pattern.contains(&new.pattern));
                    prop_assert!(by.verbs.contains_all(new.verbs.with(Verb::GrantManage)));
                    // Wherever the new grant decides for anyone, it gives no
                    // verb outside the authorizing grant.
                    let mut after = grants.clone();
                    after.push(new.clone());
                    for id in IDS {
                        for kind in [ActorKind::Agent, ActorKind::Person] {
                            let s = Subject { id: id.into(), kind };
                            let access = evaluate(&after, &s, &channel, now);
                            if access.deciding.iter().any(|d| d == "new") {
                                let before = evaluate(&grants, &s, &channel, now).verbs;
                                prop_assert!(by.verbs.union(before).contains_all(
                                    access.verbs.intersect(new.verbs)
                                ));
                            }
                        }
                    }
                }
            }

            #[test]
            fn containment_agrees_with_matching(
                outer in any_pattern(),
                inner in any_pattern(),
                channel in path(),
            ) {
                if outer.contains(&inner) && inner.matches(&channel) {
                    prop_assert!(outer.matches(&channel));
                }
            }

            #[test]
            fn verbs_round_trip_through_text(set in any_verbs()) {
                prop_assert_eq!(Verbs::parse(&set.to_string()).unwrap(), set);
            }
        }
    }
}
