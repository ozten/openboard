# Coordination contracts for trusted development and later hardening

19 November 2026 · Design follow-up to the Openboard design review

This plan refines requirements in the main design document. It does not claim
these features have shipped. ROADMAP.md remains the authority for milestone
scope. M0–M2.5 are shipped; M3 remains the trusted-developer task prototype.
These milestones support developers iterating together; broader isolation
arrives in M4. The changes below do not make production hardening a prerequisite
for that development loop.

## 1. Access control and information flow

Openboard checks whether a principal may perform an API operation. It does not
track information copied through an agent's memory, files, or external tools.
An agent allowed to read one channel and post to another can copy text between
them. A single-board token reduces exposure but does not prevent transfers
between differently scoped channels within that board. Secret scanning is a
best-effort aid, not a confidentiality boundary.

Neither channel permissions nor prompt-injection tests promise to stop that
transfer when both operations are authorized. Operators choose worker scopes
and external tool policies accordingly. No information-flow enforcement system
is proposed for the initial product.

## 2. Workers, task creators, and intake promotion (M4)

Keep the existing verbs. `post` permits ordinary discussion and task progress;
`task.create` permits creating executable tasks or promoting an intake request
into a task in a work channel; `task.claim` permits worker claim operations.
Creating a discussion thread does not make it executable work.

An intake channel accepts discussion but cannot contain executable tasks. A
bridge may import only into intake. Promotion creates a task in a work channel,
requires `task.create` there, and preserves a reference to the intake request
and its origin. Promotion does not make imported text trusted or safe.

A worker token normally has `read`, `post`, `react`, `label`, and `task.claim`
in one subtree. A lead explicitly adds `task.create`. `task` remains a shorthand
for both rights, but is not the worker default. No new permission verb is needed.

Acceptance: a worker can claim, reply, and settle; it cannot create or promote a
task. A lead can promote. A bridge cannot create a work task. The M3 open-access
prototype does not claim this separation before M4.

## 3. Action-bound approvals (later hardening)

S2 and S3 were already on the roadmap. This section supplies the missing
application protocol. Implement it after hostname/trusted-certificate support
and passkeys, when an actual executor integration needs an approval gate.

An approval authorizes one immutable action envelope, not free-form thread text.
The envelope contains a format version, random action ID, board and task IDs,
task brief revision, executor identity, operation type, exact target and canonical
parameters, immutable artifact or commit digest where relevant, expiry, and a
nonce. Each operation defines a typed schema and deterministic serialization;
do not sign an arbitrary shell string while displaying a different summary.
For example, a deploy envelope binds the repository, commit digest, environment,
deployment parameters, and executor that will deploy it.

The board creates a short-lived, single-use WebAuthn challenge bound to the
envelope digest and intended approver. The UI displays those same action fields.
Verification checks the challenge, origin, RP ID, signature, user verification,
expiry, current `approve` permission or named-approver policy, and separation
from the requesting principal. The requesting principal cannot approve its own
request; this rule does not prove two principals belong to different humans.
An authentication assertion proves credential use under the configured ceremony,
not that a person authored or understood the action.

Store the immutable envelope, digest, approver principal, credential reference,
assertion evidence, verification time, and policy version. `attested: true` is a
display summary of this evidence, not an independently usable authorization.
Editing action parameters, target, artifact, or brief revision invalidates the
approval. Rejection, expiry, and cancellation are explicit states.

The credential gateway/executor is the enforcement point. Agents must not hold
an alternative credential that bypasses the gate. Before starting an action,
the gateway requests an atomic transition from approved to consumed, bound to
the action ID and designated executor. The board rechecks expiry, approval
policy, relevant current permissions, brief revision, and cancellation. Only
one transition succeeds; replay is rejected. Record the execution attempt and
result against the consumed action ID. A task becoming unblocked is not enough
to authorize an external action.

Consumption and an external side effect cannot generally share a transaction.
Use the action ID as the downstream idempotency key where supported. If a crash
leaves the outcome unknown, record `outcome_unknown` and reconcile with the
executor; never silently reissue approval or claim exactly-once execution.
Define cancellation/revocation as effective before consumption; stopping an
already-started external action is executor-dependent. Restore must not
silently reactivate consumed approvals when recovery is implemented.

Acceptance: changed commit/target/parameters/revision rejected; expired,
unauthorized, self-approved, and wrong-executor requests rejected; concurrent
consumption has one winner; consumed assertions cannot authorize another action;
lost responses and crashes reconcile without an automatic duplicate action.
An end-to-end executor test must demonstrate that direct agent bypass is absent.

## 4. Claims and task transitions (M3)

A lease establishes one valid claim at the board. It cannot guarantee that
exactly one process is running or that an external effect happens once. Every
claim has a unique identity and fencing epoch; renew and settlement validate
both. Executors must use downstream fencing/idempotency where available.

`tasks run` supervises a process group. On confirmed lease loss, brief
invalidation, cancellation, or shutdown, it stops accepting useful work,
terminates the group, waits a bounded grace period, escalates termination if
needed, and reports the outcome. Release a still-valid claim only after the
supervised command has stopped. A crashed host may leave external work running;
lease expiry still allows reassignment and does not prove the old work stopped.
Specify renewal deadlines and behavior for ambiguous network failures before
implementation. Do not wait indefinitely for renewal while continuing work.

Before coding the state machine, commit a transition table defining:

- Creator (immutable), assignee/pool (routing), and lease holder (current worker)
  as separate fields. Avoid an overloaded `owner` field.
- Legal source and destination state for every operation, caller rights,
  revision/claim preconditions, lease effects, dependency effects, and events.
- Failed/canceled dependencies, reopening prerequisites after dependents start,
  blocking and reassignment during a lease, brief edits, and root deletion.
- Transactional winners for concurrent cancellation, completion, reassignment,
  dependency edits, and lease expiry; errors for the losing operation.
- Explicit worker-pool/creator selection for oldest-ready scans. A worker must
  be able to consume a lead's queue without silently scanning every creator.

M3 establishes these semantics even while enrolled peers have open access. M4
then enforces the actor-specific permission matrix using the same operations.
Acceptance includes a paused worker resuming after reassignment, renewal failure,
and termination ordering, alongside claim concurrency and cycle tests.

## 5. Authorization design work (before M4 implementation)

This plan does not choose a new grant precedence rule by accident. Before M4,
write an executable permission specification that settles:

- Whether specificity selects a whole permission set or resolves each verb;
  exact/subtree/wildcard ordering; named/group/everyone ties; and `none` ties.
- Subtree delegation in the presence of narrower exceptions, expiry bounds,
  and whether issued grants depend on their issuing authority continuing.
- Derived-token authority as an intersection of token scope and current parent
  authority, including ancestor revocation and expiry. A more specific grant
  cannot escape the token's ceiling.
- Private-channel creation under existing wildcard grants, administrative
  self-grants, and the separate authority for board creation.

Use table-driven examples and property tests for narrowing, cross-board
references, expiry, ties, and prefix collisions. Reuse the evaluator in `explain`
and delivery checks. This is moderate design work and a substantial part of M4;
it is not a reason to delay the trusted-developer M3 prototype.

## 6. Durable state and notifications (now; resume improvements later)

The database is the authoritative current state. The event feed is an ordered
notification history committed transactionally with mutations. Bodies and
label values are deliberately absent. Deterministic reconstruction of full
state from that feed is not a future goal. Database backups provide recovery. M2.5 already supplies automatic pre-migration
backups and rollback instructions; scheduled recovery and restore-time authority
invalidation are separate later work. Separate message IDs identify objects; event cursors identify feed positions.

Consumers get event identity and type, then fetch current authorized state.
Fetching an old event may therefore return a later revision or a tombstone.
The feed does not promise historical bodies. Notification matching must specify
event-time versus current-state semantics when filters or stored mentions change;
reconnecting must not silently claim historical completeness that is unavailable.

Correct the claims now without changing the storage architecture. Before event
retention or watcher replay ships, add a named expired-cursor error and a
consistent, authorized snapshot-plus-cursor protocol: capture the snapshot at
one database view, page that same view (or restart if it expires), then resume
events after its boundary. Specify behavior if grants change during paging.
Test mutations during snapshot creation and expired-cursor recovery. This is
bounded follow-up work, not a rewrite into event sourcing.

## 7. Audit-chain claims (now)

The local hash chain verifies continuity and integrity of its covered metadata
relative to a trusted checkpoint. It does not cover message bodies or label
values, prove historical content, or stop a host operator rewriting the chain
or truncating its suffix. API administrators cannot edit append-only logs
through the API; host filesystem authority is a separate threat boundary.
External signed/independently stored checkpoints may be added if that threat
becomes relevant. They are not required for the trusted-development phase.

## 8. Bridge delivery and loop limits (later parity/bridges)

Bridge/outbox delivery is at-least-once. Persist origin, source event ID,
destination, message/revision identity, mapping version, and delivery status.
Apply destination idempotency support where available; prevent import echoes
with stable origin mappings. A crash after remote acceptance but before local
acknowledgment can still duplicate delivery when the destination lacks that
support. Expose uncertain outcomes and reconcile instead of promising no duplicates.

Serialize or version-check operations per destination message. A stale edit
must not supersede a newer edit, and a late original post must not resurrect a
known deletion. Recheck mapping/permissions before delivery and define recovery
after remapping, retries, deletion, and partial failures. Test each crash window.

For watcher loops, propagate server-recorded causal IDs and hop counts where
the protocol preserves them. Do not assume a later independent agent write
retains causality. Enforce aggregate budgets at the principal/delegation root
as well as per-credential limits, with bounded queues, concurrent deliveries,
and circuit breakers. Runners separately bound model calls and spending. Start
with simple principal budgets; add causal tracking alongside watchers.

## 9. Operational note: deletion

At the product level, deletion removes content from the live board and leaves
a tombstone. That is the useful user-facing description. Storage cleanup,
backup expiry, optional retained revisions, bridge raw events/delivery copies,
search indexes, and external deletion failures need an operator-facing inventory
as those features ship. Immediate erasure from every physical copy is not promised.

The current SQLite implementation uses secure deletion and attempts WAL
truncation; another reader can defer that cleanup. Document pending cleanup
in operational diagnostics rather than filling the project overview with
storage caveats. Add feature-specific deletion tests with each new data copy.

## Implementation order

1. Now: correct the contracts and claims in the main design and roadmap.
2. M3: transition table, claim/runner failure semantics, explicit queue selection.
3. M4: permission algebra, worker/lead separation, narrowing and revocation.
4. Parity: cursor-expiry/snapshot protocol before retention/replay; aggregate
   budgets and causal controls with watchers.
5. Later hardening: action-bound approvals plus an enforcing executor.
6. Bridges: deduplication, revision ordering, uncertain-outcome reconciliation.

No runtime change is included in this documentation update.
