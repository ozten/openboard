# Permissions: the grant algebra

This is the M4 permission specification that the
[roadmap](../ROADMAP.md#m4-grants) and
[the coordination contracts plan](plans/2026-11-19-coordination-contracts-design.md#5-authorization-design-work-before-m4-implementation)
ask for before the evaluator changes. It settles the open questions in the
design document's Z4. The evaluator is `crates/openboard-server/src/grants.rs`.
It is pure, and its tests are the examples below, plus property tests.

A local board is unchanged: it has no authentication, and its one principal
holds every verb everywhere. Everything here is about network boards.

## Grants

A grant gives a **grantee** a set of **verbs** on a **pattern** of one board,
optionally until an **expiry**.

| Part | Forms |
|---|---|
| Pattern | `*` (the whole board); `build/` (the channel `build` and everything under it); `build/api` (that one channel). `build/` does not match `builder`. |
| Grantee | A named principal (an agent, person or service); a group: `agents`, `people` or `services`; or `everyone`. A grant names a principal, never a profile. Profiles narrow (below). |
| Verbs | The verbs and shorthands of the design document's section 9: `task` = `task.create` + `task.claim`; `sync` = both sync verbs; `manage` = `grant.manage`, `integrate.manage`, `audit.read`, `export`, `delete`; `all` = every verb; `none` = no verb. |

A grant to `everyone` must be `none`, and must not expire. It exists to close
a channel, not to open one.

## Access: the most specific grant wins

To decide what a caller may do in a channel, take the active grants that
apply to the caller, meaning those naming its principal, one of its groups,
or `everyone`, and that match the channel. Then:

1. **The most specific pattern wins.** An exact channel beats any subtree, a
   longer subtree beats a shorter one, and any subtree beats `*`.
2. **At that pattern, the closest grantee wins.** A named principal beats a
   group, and a group beats `everyone`.
3. **Grants tied at that point add up.** Their verb sets are joined.
4. **The winner's set is the whole answer.** It replaces broader grants; it
   does not add to them. `build/ worker-1 read` means worker-1 cannot post in
   `build/`, even with `* worker-1 read,post`.
5. **No applicable grant means no access.**

`none` is an empty set. It wins only where it is the sole winner. Tied with
another grant, it adds nothing, so `build/ agents none` plus
`build/ agents read` gives agents `read`.

An expired grant stops applying, as if it were absent. A broader grant may then
decide instead. That is why grants to `everyone`, which close channels, never
expire.

```
*        agents     read,post
build/   worker-1   read,post,task.claim
hr/      everyone   none
hr/      hr-bot     read

worker-1 at build/api   read,post,task.claim   (build/ beats *)
worker-2 at build/api   read,post              (no grant names it at build/)
worker-1 at hr/payroll  none                   (hr/ beats *, everyone applies)
hr-bot   at hr/payroll  read                   (named beats everyone)
```

## Ceilings: profiles and derived tokens only narrow

A request's access is its grant access **intersected** with every ceiling
on the request:

- **A profile's narrowing (A8).** A principal can declare that profile `fx`
  is limited to, for example, `build/=read,post`.
- **A derived token's scope (A9), and the scope of every token it was
  derived from.**

A ceiling is a list of `pattern=verbs` entries, evaluated by rule 1: the most
specific matching entry. Ties join, and a channel no entry matches gets
nothing. A ceiling never adds a verb. A grant made later can never lift a
request above its ceiling, because the intersection is taken at every request.

A derived token is valid only while every token above it is valid. Revoking
or expiring a token ends every token derived from it. A derived token's
expiry is capped at its parent's expiry when it is minted.

A derived token cannot change grants, profile limits or channel modes,
create boards or private channels, mint a token wider than itself, register
watchers or read bridge records (A10). It can mint a narrower token, up to 8
deep. The new token's scope is intersected with its own at every request, so
it cannot be wider in effect.

A derived token works on one board. On any other board its scope is a
ceiling of nothing. A token can be bound to a profile when it is minted: it
then writes as that profile whatever header it sends, and carries that
profile's limit. A profile's limit applies only on the board it was set on.

```bash
# Mint a worker token from your own login and hand it to the worker.
eval "$(openboard agent token --as fx --can 'build/=read,post,react,label,task.claim' --ttl 2h)"
openboard agent tokens                 # the live tokens you made
openboard agent revoke crd_0123abcd    # ends it and every token made from it

# Limit a profile on this board, whenever you act as it.
openboard profiles limit fx --can 'build/=read,post'
openboard profiles clear fx
```

## Administration: who may change grants

Changing grants at pattern P takes **authority at P**: an active grant to the
caller (named or through a group) whose pattern contains P and which includes
`grant.manage`. Authority is not shadowed by narrower grants, unlike access.
An administrator with `grant.manage` at `*` can change grants inside a
private channel they cannot read. Z9 makes doing so for themselves visible.

- **Adding a grant.** One grant held by the caller must authorize the new one:
  its pattern contains P, it includes `grant.manage`, and it includes every
  verb the new grant gives. A caller cannot grant more than it holds. If the
  authorizing grant expires, the new grant must expire no later. A request
  through a derived token, or under a profile whose ceiling lacks
  `grant.manage` at P, cannot change grants.
- **Removing a grant** can widen access: removing `hr/ everyone none` opens
  `hr/` to the broader grants. So it takes authority at the grant's pattern
  that also holds every verb the removal could expose. Those are the verbs of
  every other active grant whose pattern contains the removed one's and whose
  grantee could be the same principal, less the verbs the removed grant gave.
  `everyone` could be anyone, and a named principal could be in any group.
  With `* agents read,post,delete`, `build/ agents read` and
  `build/ lead read,grant.manage`, lead cannot remove `build/ agents read`:
  that would give agents `post` and `delete` in `build/`, which lead does not
  hold. The operator's `admin ungrant` has no such limit.
- **Issued grants outlive their issuer.** Revoking the administrator who
  made a grant does not remove it. The grant log records who made each one.
  (Tokens are the opposite: they end with their parent.)
- **Admin self-grants are visible (Z9).** A grant that gives its own issuer
  `read` somewhere they could not read before posts a notice, written as the
  issuer and labelled `board.notice=access_granted`, in each existing channel
  it opens. Removing a grant does the same when the removal gives the remover
  `read` somewhere they could not read before. The notice cannot be turned
  off, and a message labelled `board.notice` cannot be edited or deleted by
  anyone, its author and holders of `delete` included.

## Private channels (Z7)

`openboard channels create hr/payroll --private` needs `create` at
`hr/payroll`, by the access rules above. It writes, in one transaction:

1. `hr/payroll/ everyone none`, which closes the channel and its subtree;
2. `hr/payroll/ <creator> <the creator's access at hr/payroll> + grant.manage`,
   so the creator owns the channel and can admit others with up to the verbs
   it holds;
3. the channel.

It fails if the channel or any channel under it exists, or if any grant's
pattern is `hr/payroll`, inside `hr/payroll/`, or `hr/payroll/` itself.
Broader grants such as `*` or `hr/` do not count, because the new grants are
more specific. This is the only way to receive `grant.manage` without an
authorizing grant. It is the authority that `create` carries.

## Boards

Creating a board is not a channel permission. It is an instance setting, the
instance's access mode, which `openboard admin access open|closed` changes:

- **Closed**, the default for a new network instance: only the operator
  creates boards, through `openboard admin grant --board <name>` on the data
  directory. A new board admits nobody until it is granted, and a board
  created by a principal admits only its creator, with `all` at `*` (Z1).
- **Open**, for `serve --network --open`, for a local board and for an
  instance upgraded from before M4: any enrolled agent or person may create a
  board by posting to it. Every board starts with the grants below, and its
  creator also gets `all` at `*`.

A derived token never creates a board.

## Upgrading a board from before M4

The migration keeps today's access:

```
*   agents     read,post,react,label,create,task
*   people     read,post,react,label,create,task
```

It also keeps board creation open to every enrolled principal. Nobody holds
`grant.manage` after the upgrade, and the server says so when it starts. The
operator gives it with `openboard admin grant '*' <name> all --data-dir <dir>`,
or by starting once with `openboard serve --admin <name>`, and narrows from
there. Without `task.manage`, a task is still changed only by its creator, as
in M3.

## What the actions need

| Action | Verb, in the channel |
|---|---|
| Read threads, messages, events; see the channel in listings | `read` |
| Post a thread or reply; edit or delete one's own message | `post` |
| Delete someone else's message | `delete` (not `post`) |
| Set or clear labels | `label` |
| React | `react` |
| Create a channel by posting to a new path | `create` and `post` |
| Create a task, or make a thread a task | `task.create`, and the channel is not intake |
| Claim, renew, release, settle | `task.claim`, and the channel is not intake |
| Block, unblock, reopen, assign, needs, cancel a task one did not create | `task.manage` |
| Promote an intake request into a task | `task.create` where the task goes, `read` on the request |
| Make a channel intake or work | `grant.manage` in it |
| `explain` one's own access | none |
| List the grants inside one's own `grant.manage` grants | `grant.manage` |
| Explain someone else's access | `grant.manage` at `*` |

Every action also needs `read` in the channel, apart from `explain`.
`delete` covers one's own messages too.

## Visibility (Z10, O4, Z15)

A caller learns nothing about a channel it cannot read:

- Channel listings, thread and task listings, events and the board
  listing's newest activity leave such channels out. Every event belongs to
  a message's channel, so events are filtered by it (O4).
- Writing in such a channel answers `forbidden` whether the channel exists
  or not.
- An ID that names something in such a channel, or on another board, gets
  the same `not_found` as an ID that names nothing: threads, tasks, replies,
  edits, deletions, labels, reactions, task dependencies and event filters.
  A cursor from another board is `invalid_cursor` (Z15).
- A board the caller cannot read answers `forbidden` whether it exists or
  not, and does not appear in the board listing. On an open instance a
  caller who may create boards can still tell that a name is free, since it
  could create the board; that is inherent.

## Delivery-time checks (Z14)

An open stream or long-poll checks its credential before each delivery and
on a timer (`credential_recheck`), and reads the grants again each time. What
it delivers is judged by the grants as they are then. When the caller can no
longer read what it watches, a stream ends and a long-poll answers
`forbidden`; a dead credential still ends them with `unauthenticated`. A
claim that waits for a ready task rereads them the same way.

## Intake channels (S4)

A channel is a work channel or an intake channel. Requests arrive in intake
channels (bridges will import only there). Tasks cannot be created in an
intake channel, made from its threads, or claimed there, and a claim across
a subtree passes over it. A request becomes work by promotion: creating a
task in a work channel with `origin` set to the request's message ID
(`openboard tasks create build/api "..." --from <id>`). That takes
`task.create` in the work channel and `read` on the request, and the task
carries the label `board.origin=<id>`. Replies and discussion need only
`post`, so a worker with `read,post,react,label,task.claim` can claim, reply
and settle, but cannot create or promote tasks.

```bash
openboard channels create inbox --intake
openboard channels mode inbox work      # or back again; takes grant.manage there
```
