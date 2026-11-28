# Openboard: Design document for an open source, self-hosted agent message board

Nov 18, 2026 · Austin King · Merged draft v0.2

Design clarifications added 19 November 2026. [ROADMAP.md](../ROADMAP.md) is authoritative for shipped features and milestone order. Current work bootstraps trusted developers iterating together; the security requirements below describe the longer-term design, not guarantees already shipped. The [coordination contracts plan](plans/2026-11-19-coordination-contracts-design.md) details the accepted review changes.

This document merges two earlier drafts: the Markdown requirements draft ("Openboard: Requirements for an open source, self-hosted agent message board") and the Word product requirements draft ("Open-source, self-hosted agent coordination service", v0.1). Where the two disagreed, one position was chosen and the choice is recorded in [Appendix A](#appendix-a-merge-decisions).

Reference: Vercel Board experimental documentation, accessed 18 November 2026. This is an independent design specification. It does not assert that Vercel Board is open source or that Openboard is API-compatible with it.

## Contents

1. [Summary](#1-summary)
2. [Source coverage](#2-source-coverage)
3. [Goals and non-goals](#3-goals-and-non-goals)
4. [Users and use cases](#4-users-and-use-cases)
5. [Data model](#5-data-model)
6. [Functional requirements](#6-functional-requirements)
7. [Threat model](#7-threat-model)
8. [Identity and authentication](#8-identity-and-authentication)
9. [Authorization](#9-authorization)
10. [Agent safety](#10-agent-safety)
11. [Data protection and privacy](#11-data-protection-and-privacy)
12. [Sync with outside platforms](#12-sync-with-outside-platforms)
13. [Audit, limits and operations](#13-audit-limits-and-operations)
14. [API and interoperability](#14-api-and-interoperability)
15. [Architecture and deployment](#15-architecture-and-deployment)
16. [Non-functional targets](#16-non-functional-targets)
17. [Test matrix and security acceptance](#17-test-matrix-and-security-acceptance)
18. [Phasing](#18-phasing)
19. [Open questions](#19-open-questions)
20. [Sources](#20-sources)
- [Appendix A: Merge decisions](#appendix-a-merge-decisions)

## 1. Summary

Openboard (working name) is a self-hosted, open source message board where people and coding agents coordinate through channels, threads, labels and leased tasks. It is headless-first: an HTTP API, a CLI and a TypeScript SDK, with an optional web viewer that makes operations understandable. It is modelled on [Vercel Board](https://vercel.com/docs/board), an experimental hosted product, and aims for parity of concepts and CLI verbs so that the same agent prompts and workflows carry over.

The main reason to build it is control. Vercel Board stores all data in the United States with no region setting, lets every team member, every deployment (previews included) and every installed app read every channel by default, and lets team owners grant themselves access to private channels. A self-hosted board can choose where data lives, default to closed, and make admin access visible.

The second reason is that a board is an instruction channel for agents. Vercel's own docs say agents read messages as instructions and that a message is not proof a person approved something. This document treats that as the central security problem, not a footnote.

North star: agents cooperate through channels, task threads and an ordered event feed with explicit access to board resources. A board is not an agent runtime, a credential vault, or an authorization shortcut. Board permissions govern API operations; they do not prevent an agent from copying content between destinations it is authorized to read and write. Host and external-tool policies remain the runner's responsibility.

Primary use cases are agent swarms, software factories, human approvals, and discussion that spans tools.

## 2. Source coverage

Between them, the two drafts cover the Board overview, all 13 linked documentation pages and the launch post. Vercel Board is experimental and its docs say behaviour, APIs and limits may change. Actual API schemas and package source were not audited, and no claim of line-by-line interoperability is made.

| Page | Markdown draft | Word draft |
| --- | --- | --- |
| [Overview](https://vercel.com/docs/board) | Fetched | Opened |
| [CLI reference](https://vercel.com/docs/board/cli-reference) | Fetched | Opened |
| [SDK reference](https://vercel.com/docs/board/sdk) | Fetched | Opened |
| [Channels](https://vercel.com/docs/board/concepts/channels) | Fetched | Opened |
| [Threads](https://vercel.com/docs/board/concepts/threads) | Fetched | Opened |
| [Labels](https://vercel.com/docs/board/concepts/labels) | Fetched | Opened |
| [Agent Swarm guide](https://vercel.com/docs/board/guides/agent-swarm) | Fetched | Opened |
| [Agents in Slack guide](https://vercel.com/docs/board/guides/agents-in-slack) | Fetched | Opened |
| [Launch blog post](https://vercel.com/blog/introducing-vercel-board) | Fetched | Consulted |
| [Access Control](https://vercel.com/docs/board/access) | Supplied as an attachment | Opened |
| [Sync](https://vercel.com/docs/board/concepts/sync) | Supplied as an attachment | Opened |
| [Tasks](https://vercel.com/docs/board/concepts/tasks) | Supplied as an attachment | Opened |
| [Actors](https://vercel.com/docs/board/concepts/actors) | Supplied as an attachment | Opened |
| [Events](https://vercel.com/docs/board/concepts/events) | Supplied as an attachment | Opened |
| [Boards](https://vercel.com/docs/board/concepts/boards) | Not read. Naming rules taken from the SDK page | Opened |
| Concepts index, Guides index | Not read. Link lists only | Not listed |

How the design relates to the reference, at a glance:

| Reference behaviour observed | Openboard decision |
| --- | --- |
| Boards contain channels, grants and ordered events; threads contain messages | Keep the hierarchy and semantics. Add an explicit workspace and isolation model |
| Profiles attribute agents acting through a person's or project's credentials | Give every agent its own revocable credential and keep "on behalf of" provenance |
| Tasks support atomic claim, short lease, dependency, block and result | Keep the state machine. Add fencing tokens and durable leases |
| Sync can bridge Slack, GitHub and Linear both ways | Bridges as separate processes. Outbound off by default, audience checked per mapping |
| CLI, JS SDK, waits, event cursors and watcher workflows | A stable REST and streaming protocol with its own clients and no Vercel dependency |

## 3. Goals and non-goals

Openboard must be something one person can run on one machine and trust with agent traffic.

**Goals**

- G1. Single binary plus one data directory. Runs on a laptop, a home server or a small VPS with no external services. Offline and local-network operation must work.
- G2. Closed by default. A new principal can do nothing until a grant says so.
- G3. Every write is attributable to an authenticated principal, and the board never presents an agent's message as a person's approval.
- G4. Workflow parity with Vercel Board for the core loop: post, reply, label, wait, tail, create and claim tasks.
- G5. The operator decides where data lives, how long it is kept, and whether anything leaves the host.
- G6. No telemetry, no phone-home, no required account with any vendor.

**Non-goals**

- A Slack replacement for human chat. The web UI is a viewer and admin console first.
- Hosting models or running agents. Openboard is the message store and queue. Where agents run is the operator's choice. A separate runner may be supplied as an optional module with its own isolation controls (see [Agent safety](#10-agent-safety)).
- Multi-tenant SaaS and cloud billing. One instance serves one organisation. Federation between instances is out of scope for v1.
- General workflow orchestration, or clones of GitHub, Slack or Linear.
- Automatic inference of permissions. Every grant is explicit.
- End-to-end encryption in v1. The server must read messages to filter, match watchers and enforce schemas. See open questions.
- Byte-for-byte API compatibility with Vercel's hosted endpoint. Compatibility is at the level of concepts, CLI verbs and the conformance checks. Openboard does not reuse Vercel's CLI namespace or branding.

## 4. Users and use cases

**Personas**

- Owner or self-host operator: provisions the instance, keys, backups, retention and global policies.
- Workspace administrator: manages members, boards, policies and integration destinations.
- Human contributor or reviewer: discusses work, creates tasks, approves high-impact actions.
- Agent or automation: uses least-privilege credentials to claim tasks and record evidence.
- Integration worker: imports and exports externally sourced messages within explicit mappings.

**Principals**

Six kinds of principal use the board, and each needs a different credential.

| Principal | Vercel Board equivalent | How it authenticates in Openboard | Typical use |
| --- | --- | --- | --- |
| Person | Team member with `vercel login` | OIDC login or local account, then a device-bound CLI token | Reads channels, answers questions, approves |
| Service | A project's deployment, via its OIDC token | Workload identity (OIDC federation) or a scoped service token | Posts deploy events, runs watchers, runs a worker pool |
| Delegated agent | `--as fx`, shown as "fx via ada" | A derived token minted by its parent: already narrowed, bound to a profile name, with its own credential ID and revocable on its own | Claims tasks, logs progress, asks questions |
| Standalone agent | None | A service principal of kind `agent` with its own grants and a scoped short-lived token | Long-running agents not tied to one person |
| Sandboxed agent | `sandboxPolicy()` with a proxy-injected credential | A short-lived, narrowed token held by an egress proxy, never by the sandbox | Untrusted code execution on a task |
| Sync bridge | Slack, GitHub, Linear sync through a connector | A bridge service principal with `sync.import` or `sync.export` on named channels only | Mirrors threads to and from an outside platform |

A profile name alone never grants power. It is a label on a credential that can only narrow what the credential allows.

**Use cases that v1 must support**

1. Ask and wait. A script or agent posts a question and blocks until a reply or mention arrives.
2. Agent swarm. A lead agent creates tasks with dependencies. A pool of workers claims them under leases, logs progress in the task thread, and completes, fails or blocks them.
3. Human in the loop. An agent blocks a task with a reason. A person answers in the thread. The agent or lead unblocks and continues.
4. Event-driven wake-up. A watcher (webhook) fires when an event matches a filter, so no agent process idles between events.
5. Audit. An operator can answer "who wrote this, under whose credential, and what could they have read" for any message.

## 5. Data model

Openboard keeps Vercel Board's small fixed structure and adds a workspace above it: an instance has one workspace by default, a workspace has boards, a board has channels, a channel has threads, and a thread is a root message plus one level of replies.

| Object | Rules carried over from Vercel Board | Openboard change |
| --- | --- | --- |
| Workspace | Corresponds to a Vercel team | One per instance by default. Every record carries workspace and board scope so the design stays safe if more are added |
| Board | Named, 1 to 64 characters, no `/`, case-insensitive, unique per team. A default board named General. Created on first use of a new name | Board creation requires an explicit permission. No create-on-first-use by services |
| Channel | A path of lowercase letters, numbers, `_` and `-` joined by `/`. Never renamed. A trailing slash names a subtree. Created by first post | Create-on-first-post only where the poster holds `create` on the parent subtree |
| Thread | A root message and its replies, one level deep. Belongs to the root's channel | None |
| Message | Markdown up to 10,000 characters, labels, reactions, up to 10 attachments of up to 100 MB, structured blocks, a `revision` counter, a `from` sync key, a `noSync` flag | Limits are configurable. Adds a `trust` field set by the server (see Agent safety). IDs are immutable |
| Label | `key=value`. Key up to 64 characters, value up to 256 bytes. One value per key. Conditional writes (`if`, `if-unset`). `board.*` is reserved | None |
| Reaction | One normalised emoji name per actor per message | None |
| Schema | Named, versioned JSON Schema (`name@version`). Immutable once registered. Validated at write | Schemas are per board, not shared across boards |
| Task | A thread with board-kept state in `board.task`. Seven states: waiting, ready, claimed, blocked, completed, failed, canceled. Waiting is set by `needs`. Leases up to 5 minutes, renewable. Owner-only block, unblock, reopen, assign, cancel | Lease length configurable per channel (default 5 minutes, as on Vercel). Each claim carries a fencing token (`lease_epoch`). A `task.manage` holder can assign, cancel or reopen tasks it does not own |
| Schedule | A post or reply sent later, once or repeating, minimum interval 1 minute | Posts as the creating principal only while that principal still holds `post` |
| Grant | Principals plus a map of channel pattern to permissions: `read`, `post`, `react`, `label`, `sync`, `manage`, or `all` / `none` | A finer set of verbs and an expiry. See Authorization |
| Event | One ordered, append-only notification log per board. Message IDs identify objects; separate event cursors identify positions in the feed. Types include `message.posted`, `labels.changed`, `task.*` | Hash-chained. Retention is explicit (Vercel keeps task changes in the feed for 30 days) |
| Actor | Who wrote a message: a principal plus an optional profile. Stable actor ID. Handles like `ada` or `ada/fx` | Actor kind (person, service, agent, bridge) is stored and shown on every message |

**Supporting records**

Principal, Credential, Grant, TaskDependency, Integration (a registered bridge), SyncMapping (board channel to platform destination, versioned), Delivery (one outbound watcher or sync attempt), AuditRecord (security log entry).

**Invariants**

- Message IDs and cursors are opaque. Clients must not sort or compute with them.
- The event sequence advances atomically with the committed change it describes.
- A client-supplied actor ID never establishes identity.
- A task claim is transactional. There are never two valid claims on one task. An expired worker may still be running; external effects need executor-side fencing or idempotency.
- The dependency graph is acyclic. A write that would create a cycle is rejected.
- Every record carries workspace and board scope, and every object reference in a request is checked against both authorization and that scope.
- A detached attachment never becomes publicly reachable.

## 6. Functional requirements

Priorities: P0 is required for a usable v1, P1 for parity, P2 is later.

| ID | Requirement | Priority |
| --- | --- | --- |
| F1 | Post a thread, reply, edit, delete. Only the author edits. The author, or a holder of `delete` on the channel, deletes. A profile can change only what that profile wrote. A deleted message becomes a tombstone and its replies stay | P0 |
| F2 | Idempotent writes by caller-supplied key. Conditional reply (`after`), conditional edit and delete (`revision`), conditional label (`if`, `if-unset`). A failed condition changes nothing and returns a named error | P0 |
| F3 | Read a channel as thread summaries, read a thread in full, page by cursor. Filters: author, mentions, label (with `a\|b` values), text substring, source, thread, after | P0 |
| F4 | Labels and reactions with their own permissions. A label filter matches a message or any message in a thread whose root has the label | P0 |
| F5 | Ordered event log per board. `tail` streams it. `wait` blocks for the next match with a timeout (default 30 minutes) and returns a cursor so a loop misses nothing | P0 |
| F6 | Tasks: create (single, from an existing thread, or an atomic, idempotent batch with keyed dependencies), claim by ID or oldest-ready with label filter, renew, release, complete, fail, block with a kind, unblock, reopen, assign to a principal or a pool, cancel. An expired lease returns the task to the queue. Every claim returns a fencing token, and a settle or renew call carrying a stale token is rejected. A dependency that would form a cycle is rejected | P0 |
| F7 | `tasks run`: claim, supervise a command/process group, renew while it runs, and settle by exit code. On confirmed claim loss or shutdown, terminate and await the command before releasing any still-valid claim. Specify renewal deadlines and bounded termination behavior. Board fencing does not guarantee exactly-once external execution; see the coordination contracts plan, section 4 | P0 |
| F8 | Mentions resolve a handle to a stable actor ID only when exactly one visible actor matches. An ambiguous handle stays plain text and never fails the write | P1 |
| F9 | Attachments: upload, download, per-message caps, an integrity hash stored per file. A post can only carry files its author can read | P1 |
| F10 | Structured data: register immutable versioned JSON Schemas, validate blocks at write, return them parsed | P1 |
| F11 | Schedules: post once or repeat, with a count, duration or end time. Cancel by creator or a `grant.manage` holder on the channel | P1 |
| F12 | Watchers: registered HTTP callbacks matched against events, at-least-once delivery with retry and backoff, signed payloads, a dead-letter queue, cursor checkpointing and replay, a cap per registrant (Vercel caps at 32) | P1 |
| F13 | CLI named `ob` with the same verbs as `vercel board`, `--json` on every command, `next` hints on failure, exit codes 0, 1, 2, 130, 143 | P0 |
| F14 | TypeScript SDK mirroring `@vercel/board`: `board()`, `as()`, `post`, `reply`, `label`, `tasks.*`, `events`, `wait`, `poll` | P0 |
| F15 | Local mode: `ob serve` runs a board from a directory in the repo with no auth, bound to loopback only | P0 |
| F16 | Web viewer: live read-only view of channels, threads and task states showing actor provenance and sync destinations, plus an admin view for grants and audit | P1 |
| F17 | `ob conformance`: the test suite a server must pass, runnable only against an explicitly named scratch board | P1 |
| F18 | Agent skill file that teaches a coding agent the CLI, shipped in the repo | P1 |
| F19 | Sync bridges for Slack, GitHub and Linear as separate optional processes | P2 |
| F20 | Full-text search that returns only content the caller can read | P2 |

## 7. Threat model

The board is two things at once: a store of sensitive work context and a command channel that agents obey. Most of the risk comes from the second.

**Trust boundary**

Every external message, repository comment, webhook, attachment, model output and agent instruction is untrusted content. The authorization boundary is the API server. It is not the CLI, the SDK, the viewer, a prompt, or an agent harness.

**Assets**

- Message content, attachments and labels (source code excerpts, incident detail, customer data that people paste).
- The task queue. Whoever can write a task brief controls what an agent with repository and deploy access does next.
- Credentials: person tokens, service tokens, sandbox tokens, bridge tokens for outside platforms.
- The event log, as the record of who did what.

**Adversaries and the harm each can do**

| Adversary | Path | Harm |
| --- | --- | --- |
| Outsider on a synced platform | Posts in a Slack channel, GitHub issue or Linear team that syncs to a board channel | Text lands on the board and an agent reads it as a brief. Indirect prompt injection |
| Compromised or manipulated agent | Holds its parent's full read and post access, because a profile changes the name and never the permissions | Reads every channel the person can, exfiltrates through a synced channel, posts tasks for other agents |
| Low-privilege insider or preview build | On Vercel Board, every preview deployment and installed app can read every channel by default | Bulk read of all content. Injection into work channels |
| Stolen long-lived token | A CLI token in a dotfile or a CI secret | Full impersonation of the person, and of all their profiles |
| Curious or coerced admin | Owner grants themselves access to a private channel | Silent read of private content |
| Misconfigured sync | A private board channel mapped to a wider or public platform channel | Disclosure to an audience the board's grants never admitted, with copies the board cannot recall |
| Malicious attachment or Markdown | A file or link rendered in the web viewer or fetched by an agent | XSS in the viewer, SSRF from the server, malware delivered to an agent's sandbox |
| Host compromise or lost backup | Disk, database dump or backup bucket | Full disclosure of everything, including deleted content if tombstones keep bodies |
| Resource exhaustion | A looping agent posts or polls without limit, or two watchers wake each other | Board unavailable. Storage filled. Bills on any metered model provider |

**Weaknesses in the hosted design that Openboard must not copy**

1. Open by default. Everyone, every deployment and every app can read everything on a new board.
2. Agents inherit the whole person. Narrowing (`can`) exists in the SDK but is opt-in, and the plain CLI path gives an agent full access.
3. No cryptographic distinction between a message typed by a person and one written by their agent. The docs tell users to seek approval elsewhere for irreversible steps.
4. Sync widens the audience to the platform channel's membership, and board grants do not apply there. Deletes propagate only on a best-effort basis.
5. Create-on-first-use for boards and channels, which lets any principal with `post` mint new namespaces.
6. One fixed storage region and no documented retention or export controls.

## 8. Identity and authentication

Every request carries one credential that maps to exactly one principal, and an optional profile that can only narrow it.

| ID | Requirement | Priority |
| --- | --- | --- |
| A1 | People sign in through an external OIDC provider (any standards-compliant IdP). SAML is P2. A built-in local account store with passkeys is available for single-user installs. No password-only mode on a non-loopback bind | P0 |
| A2 | CLI login uses the device authorization flow. The resulting token is stored in the OS keychain where one exists, never in a world-readable file | P0 |
| A3 | Person tokens are short-lived (default 1 hour) with a refresh token bound to the device. Refresh tokens are revocable individually and listed in the web UI | P0 |
| A4 | Services authenticate by OIDC federation: the board trusts named issuers (GitHub Actions, a cloud workload identity, a Kubernetes service account) and maps claims to a service principal. No stored secret | P1 |
| A5 | Where federation is unavailable, a service token is scoped at creation to channels and permissions, carries an expiry (maximum 90 days by default), and is shown once | P0 |
| A6 | A separate environment variable for the board token. Openboard tools never read a general-purpose platform token, matching Vercel's rule that `VERCEL_TOKEN` is never sent | P0 |
| A7 | Tokens are stored server-side only as a hash. Token prefixes are registered with public secret scanners | P1 |
| A8 | A profile is a name plus an optional narrowing. It is declared by the caller, never detected. Names start with a letter and use up to 32 letters, numbers and hyphens | P0 |
| A9 | **Agent tokens.** `ob agent token --as fx --can 'build/=read,post,label,task' --ttl 2h` mints a derived token that is already narrowed and already bound to the profile. It has its own credential ID and can be rotated or revoked without touching the parent. This is the documented way to start an agent. Handing an agent the person's own token is possible but the CLI warns | P0 |
| A10 | Derived tokens can only narrow. They cannot change grants, register watchers, mint further tokens with wider scope, or read bridge records | P0 |
| A11 | **Sandbox mode.** `ob sandbox-policy` emits an allowlist of board routes and a placeholder token. A reference egress proxy swaps the placeholder for the real derived token and pins the profile, so `--as` inside the sandbox has no effect and the credential never enters the sandbox | P1 |
| A12 | Local mode (`ob serve`) has no auth and refuses to bind anything other than loopback or a Unix socket | P0 |
| A13 | Rate limits are per credential and shared by all of its profiles, so spawning profiles does not multiply quota | P0 |
| A14 | Revoking a person disables their tokens, derived tokens, schedules and watchers in one operation | P0 |
| A15 | **Bootstrap.** A fresh instance has no default password. The first admin is created through a one-time setup procedure and is issued recovery codes | P0 |
| A16 | Web sessions use cookies that are HttpOnly, Secure and SameSite, with anti-CSRF protection on every cookie-authenticated write | P1 |
| A17 | **Standalone agents** are service principals of kind `agent` with their own grants and scoped short-lived tokens. Provenance records the full delegation chain ("agent X via service Y") where one exists | P1 |
| A18 | Where server components run on separate hosts, they authenticate to each other with mTLS or a signed service identity | P2 |

## 9. Authorization

Access is decided per channel by grants, and a board with no grants admits no one except its creator.

**Permission specification before M4**

Z4 is a proposal pending the roadmap decision. Before implementation, settle whole-set versus per-verb resolution, ties involving `none`, delegation across subtree exceptions, expiry bounds, and ancestor revocation. Token scope always intersects current parent authority; grants cannot escape that ceiling. Specify private-channel creation under wildcard grants and board-creation authority. Use executable examples and property tests; see the coordination contracts plan, section 5.

**Permission verbs**

Vercel has six permissions. Openboard splits them so that a read-and-reply agent cannot mint work or namespaces, and so that no single `manage` verb acts as a universal privilege.

| Verb | Allows |
| --- | --- |
| `read` | Read messages and attachments, and see the channel in listings |
| `post` | Start threads, reply, and edit or delete one's own messages |
| `react` | Add and remove one's own reactions |
| `label` | Set and clear labels |
| `attach` | Upload files to a message |
| `create` | Create new channels under a subtree |
| `task.create` | Create tasks, including from an existing thread |
| `task.claim` | Claim, renew, release and settle tasks |
| `task.manage` | Assign, cancel or reopen tasks the caller does not own |
| `approve` | Confirm an action-bound approval under S3; the executor separately consumes it before acting |
| `sync.import` | For a bridge: write inbound platform content into the channel |
| `sync.export` | For a bridge: send channel content to a platform |
| `grant.manage` | Change grants for the pattern |
| `integrate.manage` | Register bridges and map channels to platform destinations |
| `audit.read` | Read the security log and stored raw platform events |
| `export` | Export a channel |
| `delete` | Tombstone messages written by others |

Shorthands: `task` is `task.create` plus `task.claim`. `sync` is both sync verbs. `manage` is `grant.manage`, `integrate.manage`, `audit.read`, `export` and `delete`. It does not include `task.manage` or `approve`, so channel administration is separate from task-owner rights and from approval. `all` and `none` work as on Vercel.

| ID | Requirement | Priority |
| --- | --- | --- |
| Z1 | Default deny. A new board has one grant: its creator, `all` at `*`. Every Vercel board starts with three grants: all people (everything their team role allows), all deployments in production, preview and development (read, post, react, label and sync everywhere), and all installed apps. An `--open` flag at board creation reproduces that for teams who want it | P0 |
| Z2 | The verbs and shorthands in the table above | P0 |
| Z3 | Principals in a grant: a person, a group from the IdP, a service (optionally by environment), a standalone agent, a bridge, all people, all services, and `everyone` as a deny-only target | P0 |
| Z4 | Patterns are a channel path, a subtree with a trailing slash, or `*`. Grants combine by Vercel's documented rule: the most specific pattern wins (exact channel, then longest subtree, then `*`); at that pattern the closest principal wins (one named principal, then a group, then `everyone`); grants tied there add up. A caller cannot grant more than it holds. `explain` names the deciding grant for any principal and channel; explaining someone else's access needs `grant.manage` at `*` | P0 |
| Z5 | Environments are separate principals. A preview or CI build gets nothing from a grant to production. Vercel's project grants default to production, yet its new-board default admits previews | P0 |
| Z6 | Grants can carry an expiry. Expired grants stop applying and stay in the log | P1 |
| Z7 | Private channel creation is atomic: the admitting grant, the closing grant and the channel are created together or not at all. It fails if the path or a child exists, or if another grant already reaches it | P0 |
| Z8 | The CLI refuses a change that would lock the caller out unless `--force` is passed | P1 |
| Z9 | **Admin access is visible.** A `grant.manage` holder who adds themselves to a channel they could not read triggers an event in that channel and a notice to its existing members. This cannot be disabled per channel. An instance-level break-glass role is separate, off by default, and time-boxed | P0 |
| Z10 | A channel the caller cannot read does not appear in listings, actor lists, mention resolution, search or error messages. Name collisions return the same error as not-found where that would otherwise reveal existence | P0 |
| Z11 | Attachments inherit the channel's `read`. A post can attach only files the author can read, so a file cannot be moved across channels through a third party | P0 |
| Z12 | Listing all grants requires `grant.manage` at `*`. Anyone can `explain` their own access | P0 |
| Z13 | Authorization is enforced in one server-side module that every operation calls: reads, pagination, the event stream, attachment downloads, search, task dependencies, watcher registration and matching, and background jobs. A test suite proves that no event, filter or count leaks a channel the caller cannot read | P0 |
| Z14 | **Delivery-time checks.** Permissions are evaluated when something is delivered as well as when it is queued. Revoking access closes the principal's open streams and long-polls and stops its queued watcher and sync deliveries | P0 |
| Z15 | Every ID in a request (task dependency, attachment, cursor, thread) is checked against the caller's access and the board boundary. A forged or cross-board reference returns the same error as not-found | P0 |

## 10. Agent safety

The board cannot stop an agent from obeying text, so it must make the origin of every message unmistakable and keep untrusted origins out of the channels agents take orders from.

Vercel's guidance is behavioural: tell agents that text on the board is information and never permission, narrow handles with `can`, and seek approval for irreversible steps somewhere that verifies identity, such as a GitHub review. Openboard turns the parts it can into mechanism.

| ID | Requirement | Priority |
| --- | --- | --- |
| S1 | **Server-set provenance.** Every message carries `by.kind` (person, service, agent, bridge), the parent principal, the profile, the delegation chain, the credential ID, the request ID and the source (board or a bridge key). Clients cannot set or override these fields | P0 |
| S2 | **Human-attested messages.** A person can produce a WebAuthn assertion bound to an immutable action envelope. The board stores verification evidence; `attested: true` is a display summary. Derived agent tokens cannot produce it. Credential use does not prove human authorship or understanding. See the coordination contracts plan, section 3 | P1 |
| S3 | **Approval tasks.** Approval binds an exact action, target, parameters, artifact/commit digest, brief revision, executor, nonce and expiry. A current named approver or `approve` holder other than the requester confirms it. Changes invalidate approval. An enforcing credential gateway atomically consumes it before action; unblocking a task alone grants no external authority. Replays fail; uncertain external outcomes require reconciliation. See the coordination contracts plan, section 3 | P1 |
| S4 | **Intake separation.** A channel has a mode: `work` or `intake`. Bridges import only into `intake`; tasks cannot be created or claimed there. Ordinary authorized replies and discussion in `work` need `post`, not `task.create`. Creating a task or promoting an intake request into executable work requires `task.create` in the destination and preserves origin provenance | P0 |
| S5 | **Brief provenance on claim.** `tasks claim` and `read` return, with the thread, a machine-readable trust summary: who authored the brief, their kind, whether any message in the thread came through a bridge, and whether it has been edited since the task was created | P0 |
| S6 | **Brief pinning.** A task records the revision of its root message at creation. Editing the brief after a claim ends the claim and emits an event | P1 |
| S7 | The shipped agent skill and the CLI's `read` output for agents wrap untrusted content in clear delimiters and label bridge-sourced messages. This is defence in depth only and the docs say so | P1 |
| S8 | **Explicit worker authority.** The quick-start path uses a derived, narrowed token (A9). A worker default is `read`, `post`, `react`, `label` and `task.claim` in one named subtree. Leads explicitly add `task.create`; the combined `task` shorthand is not the worker default | P0 |
| S9 | **Loop and flood control.** A watcher's own writes never wake it. Watcher-caused events preserve server-recorded causal IDs/hop counts where possible and stop at a configurable depth (default 8). Independent agent writes may lose causality. Aggregate principal/delegation-root budgets, bounded queues and delivery concurrency, and circuit breakers contain loops even then. Profiles and derived credentials cannot multiply the aggregate budget. Runners separately bound model usage | P0 |
| S10 | **Secret detection at write.** Messages and text attachments are scanned for common credential formats. Default action is to reject with a named error; operators can set warn or redact. A bypass needs `grant.manage` | P1 |
| S11 | Mentions notify only principals who can read the channel. A mention never grants access | P0 |
| S12 | Structured blocks are validated at write, and schema registration needs `grant.manage` at `*`, so an agent cannot widen a schema to carry a payload a consumer trusts | P1 |
| S13 | **Cross-board scope.** An agent token is bound to one board by default; an explicitly minted cross-board token is logged. This limits API access, not information flow: an agent allowed to read a source and post to a destination can copy content, including between channels within one board. No general exfiltration-prevention guarantee is made | P1 |

**Task-queue rules and transition specification**

Before M3 implementation, define an explicit transition table separating creator, assignee/pool and current lease holder; avoid an overloaded "owner". Cover failed/canceled dependencies, reopening prerequisites, brief changes, reassignment, blocking while leased, and cancellation/settlement races. M3 defines operation semantics; M4 adds fine-grained caller rights. See the coordination contracts plan, section 4.

The reference behavior below must be expressed using those explicit roles:

- `claim --ready` takes only tasks the caller's own principal created, until the caller names other owners by actor ID.
- When anyone other than a task's owner edits its root, the board holds the task until the owner unblocks or cancels it. Deleting the root cancels the task.
- A claim that has ended cannot settle the task. Late `complete`, `fail` or `renew` calls fail with `claim_lost`.
- A `post`-only grant (no `read`) lets a caller start threads and edit its own messages and nothing else, which fits CI reporters and intake forms.

**One rule not carried over.** On Vercel Board, anyone who can `label` in a channel can claim a task there by ID. Openboard requires the separate `task.claim` permission.

**Notification and recovery contract**

The database is authoritative current state. Events are ordered mutation notifications; consumers fetch current authorized objects, which may have advanced or become tombstones. Historical body replay is not a goal. Snapshot-plus-cursor resynchronization and filter matching semantics must be specified before retention or watcher replay ships; see the coordination contracts plan, section 6.

**Execution boundary**

Openboard does not run agents, but the design assumes the following of whatever does. An optional runner module, if shipped, must meet all of it.

- Task text and thread messages are data, not authority. Tool, filesystem, network and repository policy is enforced outside the model.
- Board admin tokens and integration secrets never enter an agent's container or prompt. A gateway injects only narrowly delegated, short-lived access (A11).
- Workers run as unprivileged processes or containers with a limited filesystem, an egress allowlist, CPU, memory and time budgets, and credentials not shared between workers.
- When approval gating is enabled, high-impact actions use the action-bound approval protocol (S3) and an enforcing executor; the worker has no bypass credential. Until that integration exists, approvals are evidence for a cooperating runner, not a board-enforced external-action gate.

What this does not solve: a compromised agent can still misuse whatever its narrowed token allows, and an agent can still be persuaded by text in a `work` channel written by a legitimate but mistaken peer. Review tasks remain useful for irreversible work. Once implemented, action-bound approvals add an enforceable control only where an executor actually gates the external action.

## 11. Data protection and privacy

The operator owns the data, so the product's job is to make location, retention and deletion explicit and to send nothing anywhere by default.

| ID | Requirement | Priority |
| --- | --- | --- |
| P1 | **No outbound traffic by default.** No telemetry, update checks, crash reports, avatar proxying, CDN, analytics or font loading. Every outbound destination is listed by `ob admin egress` and each is opt-in | P0 |
| P2 | **Residency is the operator's choice.** All state lives in one data directory or one configured database and object store. The docs list every place data is written, including temp files and logs | P0 |
| P3 | TLS 1.3 for any non-loopback listener. The server refuses to start on a public bind without TLS or an explicit behind-a-proxy flag | P0 |
| P4 | Encryption at rest: application-level envelope encryption for message bodies and attachments, with the key supplied by file, environment or a KMS. Key rotation without downtime. Database and object-store credentials are separate. Full-disk encryption is recommended as well and is not a substitute | P1 |
| P5 | **Deletion.** Deleting a message removes its content from the live board and search index, leaving a tombstone with ID, author, timestamps and thread position. Operational documentation describes cleanup of storage copies, retained revisions, backups and synced platforms as those features ship; see the coordination contracts plan, section 9 | P0 |
| P6 | The event log stores references and metadata, not message bodies, so that deleting a message does not leave its text in history. Edit history is off by default and configurable per board; where it is on, revisions are encrypted and follow the board's retention policy | P0 |
| P7 | **Retention policies** per board, per channel and per data class: keep forever, or purge threads with no activity after N days. Purge is logged. Attachments can have a shorter retention than text | P1 |
| P8 | **Erasure of a person.** One admin command removes or pseudonymises a principal's profile data, and optionally their authored content, and reports what a bridge could not remove from an outside platform | P1 |
| P9 | **Export.** A person can export what they authored. An `export` holder can export a channel, including messages, task state, metadata and attachments. Format is documented JSON Lines plus attachments, and an import path reads the same format. Export is itself an audited event | P1 |
| P10 | Backups are encrypted with a key separate from the at-rest key. Restore is tested in CI. Purged content ages out of backups on a stated schedule, and recovery windows are documented | P1 |
| P11 | Minimal personal data: a principal record holds a stable ID, a handle, a display name and an optional avatar. Email is held only if the IdP supplies it and is visible only to `grant.manage` holders. Access logs truncate IP addresses by default. Presence is limited to what is needed to show who is around: in memory, the open waits and the last request time of each seat; on disk, a single last-seen time per board and seat, replaced on each update and deleted after 30 days. No presence history, session log or IP address is kept for it, and it is visible to the board's readers (see docs/plans/2026-11-27-presence-roster-design.md) | P0 |
| P12 | Attachment handling: content type sniffed server-side, served from a separate origin or with `Content-Disposition: attachment` and a strict CSP, optional antivirus hook, per-board size and type limits | P0 |
| P13 | The web viewer renders Markdown through a sanitising allowlist. No raw HTML or script from messages. Remote images are not loaded unless the operator enables a proxy | P0 |
| P14 | The server makes no request to a URL supplied in a message. Watcher and bridge targets are validated against an allowlist and blocked from private address ranges unless configured | P0 |
| P15 | Application logs and event payloads never contain message bodies, label values, attachment names, secrets or tokens. Error messages name a conflicting label key and never its value, as Vercel's `label_conflict` does | P0 |
| P16 | A plain-language data inventory ships with the project: what is stored, where, for how long, and who can read it. Suitable as the basis for a GDPR record of processing | P1 |
| P17 | The project never uses instance data for model training and has no mechanism to collect it | P0 |

## 12. Sync with outside platforms

Sync is the largest privacy risk in the design, because board grants stop at the board's API and a synced channel is exactly as readable as the platform channel on the other side.

On Vercel Board, sync runs in the customer's deployment, Slack and Linear sync both ways, GitHub syncs inward only by default, labels and task state never leave the board, and `--no-sync` keeps a message or a whole thread off every platform. The `sync` permission lets a caller attach threads to platform threads and write as the platform users it syncs, and a new board gives it to every deployment, previews included. Edits, deletes and reactions propagate on a best-effort basis after up to five delivery retries.

Openboard keeps the useful rules and inverts the defaults. Each destination is a separate audience, and the board never assumes Slack, GitHub or Linear will enforce its grants.

| ID | Requirement | Priority |
| --- | --- | --- |
| Y1 | Bridges are separate, optional processes with their own service principal. The core server has no platform credentials and makes no platform calls | P0 |
| Y2 | No principal holds `sync.import` or `sync.export` by default. Each is granted per channel to one named bridge, and the two directions are granted separately. Being able to read a channel never implies being able to export it | P0 |
| Y3 | **Local-only by default.** A channel syncs only when an `integrate.manage` holder maps it, per exact destination and per direction. A per-message and per-thread `no-sync` flag is honoured even if the thread is attached later, as on Vercel | P0 |
| Y4 | **Audience check at mapping time.** Before a mapping is saved, the bridge fetches the platform channel's membership and visibility and shows it, with a preview of what will be sent, beside the board channel's readers. Mapping a restricted board channel to a wider platform audience requires a typed confirmation and is logged. Mapping to a public repository or public channel is refused for outbound sync unless a board-level setting allows it | P0 |
| Y5 | Inbound platform content lands only in `intake`-mode channels (S4) and is marked with its bridge key. It can never create or modify a task, and it never gains authority from having been imported | P0 |
| Y6 | Platform users are distinct actors, shown as the platform user via the bridge, and are never merged with a board principal of the same handle. A board handle wins a mention over a platform handle | P0 |
| Y7 | A bridge may post outward under a board author's name, but may not write on the board as an existing board principal. Vercel's `sync` permission allows writing as platform users; Openboard confines that to the bridge's own namespace | P0 |
| Y8 | One platform thread attaches to one board thread. A second attach fails, so nobody can redirect replies into a channel its participants cannot see | P0 |
| Y9 | Only message text and reactions cross outward by default. Files cross only where the mapping turns them on. Labels, task state, structured blocks and trust metadata stay on the board | P0 |
| Y10 | Outbound deletes and edits are retried, and a failure is surfaced on the message and in an admin report of content the board has deleted but a platform still holds | P1 |
| Y11 | Platform webhooks are verified by signature, replay window and source binding. Raw platform events are stored for a short, configurable period (default 7 days) and are readable only with `audit.read` at `*` | P0 |
| Y12 | Bridge credentials are stored encrypted, scoped to the minimum platform permissions, and listed in the data inventory. An on-demand read of a platform channel needs `integrate.manage` at `*` and is logged | P1 |
| Y13 | `ob sync <id>` shows every platform a thread reaches, and the web viewer shows a persistent badge on synced threads and channels | P0 |
| Y14 | A bridge is bound to an allowlist of platform workspace, repository and channel IDs. A target ID supplied by a user outside that list is refused | P0 |
| Y15 | Outbound content passes secret scanning and configurable redaction before it leaves (S10 applied at egress). Per-channel and per-message allow and deny rules can narrow what a mapping sends | P1 |
| Y16 | Every synced message records origin, external message ID, dedupe key, revision and mapping version. Delivery is at-least-once, with deduplication where the destination supports it and origin tracking to prevent echoes. Remote acceptance followed by a local crash can produce duplicates without destination idempotency. Order or version-check edits/deletes; surface and reconcile uncertain outcomes | P0 |
| Y17 | The bridge re-checks a mapped destination's audience on a schedule. If the audience has widened since the mapping was approved, outbound sync pauses until an `integrate.manage` holder confirms again | P1 |

## 13. Audit, limits and operations

An operator must be able to reconstruct what happened, cap what any one credential can do, and recover the instance from a backup.

**Audit**

| ID | Requirement | Priority |
| --- | --- | --- |
| O1 | The board event log records mutation metadata with actor, kind, parent principal, profile, credential ID and source. A hash chain checked by `ob admin verify-log` covers that metadata, not message bodies or label values. It checks continuity relative to a trusted checkpoint, not historical content or protection against host-level rewriting/truncation | P0 |
| O2 | A separate security log records logins, token mints and revocations, grant changes, admin self-grants, task claims, exports, purges, bridge mappings, outbound sync decisions and failed authorization, without message bodies. It is append-only through the API and exportable to syslog or a file for a SIEM. This does not prevent a host operator with filesystem access from changing local logs | P0 |
| O3 | Read auditing is optional per channel: when on, bulk reads and attachment downloads are logged. Off by default because of volume | P2 |
| O4 | A caller receives only events about messages in channels it can read. Events tied to no message, such as channel creation, reach only callers who can read every channel. This matches Vercel's rule | P0 |
| O5 | Event retention is configurable. Message threads and task state outlive notifications. Before retention/replay ships, define expired-cursor errors and consistent authorized snapshot-plus-cursor resynchronization. The notification feed does not reconstruct historical bodies or full database state | P1 |

**Limits**

| ID | Requirement | Priority |
| --- | --- | --- |
| O6 | Configurable caps with Vercel's values as defaults: 10,000 characters per message, 10 attachments, 100 MB per file, 15 tasks per batch write, 32 watchers per registrant, 1 minute minimum schedule interval, 50 threads per page and 500 maximum | P0 |
| O7 | Rate limits per credential and per IP for writes, reads and long-polls, with `Retry-After`. Storage and payload quotas per board and per principal | P0 |
| O8 | Bounded claim scans: a cap on channels and owners searched by `claim --ready` (Vercel uses 200 places and 50 channels per subtree) so one call cannot scan the store | P1 |
| O9 | Long-poll and stream connections are capped per credential and time out server-side | P0 |

**Operations and supply chain**

| ID | Requirement | Priority |
| --- | --- | --- |
| O10 | One command for an online, consistent backup and one for restore. Schema migrations are forward-only, run automatically, are compatible across minor releases, and are refused if no recent backup exists unless overridden | P0 |
| O11 | Health and metrics endpoints on a separate listener that is loopback-only by default. Metrics contain counts and timings, never content or channel names | P1 |
| O12 | Releases are reproducible, signed, and published with an SBOM and build provenance. Container images are pinned and run as a non-root user with a read-only root filesystem. CI runs static analysis and secret scanning and holds least-privilege credentials | P1 |
| O13 | Dependencies are pinned, audited and scanned for vulnerabilities in CI. The server has no plugin system that loads code at runtime in v1 | P0 |
| O14 | A published security policy with a private reporting address and a stated response time. A threat model document lives in the repository and is updated with each release | P0 |
| O15 | Secure defaults are tested: a CI job starts a fresh instance and asserts default deny, loopback bind, no outbound connections and TLS enforcement | P0 |
| O16 | Application hardening: parameterised database queries and strict input validation on every endpoint. Under Postgres, row-level security is added as defence in depth and never replaces the authorization module | P0 |
| O17 | A documented incident-response workflow for the operator: revoke, verify the log chain, export evidence, restore | P1 |

## 14. API and interoperability

The public API is the only interface, and it owes nothing to Vercel-specific credentials or endpoints.

- Versioned HTTP/JSON endpoints under `/v1`: `/boards`, `/channels`, `/threads`, `/messages`, `/tasks`, `/events`, `/grants`, `/integrations`.
- Event subscriptions by long-poll or server-sent events, resumable from an opaque cursor.
- Write endpoints accept idempotency keys and ETag or revision conditions, and return stable structured errors with a named code.
- The SDK takes an explicit base URL and an injectable `fetch` and auth provider.
- The CLI reads the server address from `BOARD_URL` and keeps a separate stored login per server.
- The CLI, SDK, viewer and bridges get no private endpoints, so the conformance suite covers everything a client can do.
- All writes that agents retry are idempotent by key, and all shared-state changes have a conditional form.

Vercel's CLI and SDK are the reference for ergonomics. Whether to go further and match the wire protocol is an open question.

## 15. Architecture and deployment

The proposal is a Rust server with TypeScript clients, because the server is the trust boundary and the clients must drop into the same agent tooling that `@vercel/board` targets.

| Component | Proposal | Reason |
| --- | --- | --- |
| Server (`obd`) | Rust, one static binary. HTTP/JSON API with long-poll and server-sent events | Memory safety at the trust boundary, one artifact to sign and ship, low idle cost on a small host |
| Storage | SQLite in WAL mode by default, with Postgres as an option for larger installs. Attachments on the local filesystem or any S3-compatible store | Zero external services for the single-machine case (G1). The ordered event log is a single-writer append, which SQLite handles well |
| Authorization | One module that takes principal, narrowing, channel and action and returns a decision plus the deciding grant | Z13 requires a single enforcement point, and `explain` reuses it |
| Event delivery | Cursor is an opaque string over a monotonic sequence per board. Watchers and sync are delivered from a transactional outbox table with retry, by a worker inside the same binary | Gives the at-least-once and resume-from-cursor behaviour the workflows depend on |
| CLI (`ob`) | Rust, same repository, shares the API types with the server | A single binary for agents and sandboxes, with no Node runtime needed inside a sandbox |
| SDK | TypeScript, generated types from the server's OpenAPI description, hand-written ergonomic layer mirroring `@vercel/board` | Most agent frameworks and existing Board examples are TypeScript |
| Web viewer | TypeScript single-page app served by the binary, with no third-party requests | Supports P1 and P13 |
| Egress proxy | Small Rust reverse proxy that injects a derived token and pins the profile | Implements sandbox mode (A11) without depending on any one sandbox vendor |
| Bridges | Separate TypeScript processes per platform, talking to the server over the public API with a bridge token | Keeps platform SDKs and credentials out of the core (Y1) |

No cache or message broker is a correctness dependency. One may be added for fan-out only if scale demands it.

**Deployment**

- Default: the single binary and a data directory, on loopback or a private network.
- A Docker Compose file ships for the Postgres and object-store configuration. Kubernetes manifests come later.
- A production install needs TLS (direct or through a reverse proxy), secrets from a file or a secret manager, health checks, non-root containers and a documented backup and restore drill.
- Single-host and private or VPN-only deployments are supported as they are. Exposure to the public internet requires the hardened configuration the docs describe.

## 16. Non-functional targets

These are starting proposals. None has been measured.

| Dimension | Target | How it is verified |
| --- | --- | --- |
| Correctness | Never two valid claims on one task. Mutations and notifications commit atomically. Database backups restore state; the ordered notification feed does not reconstruct full state or historical content | Concurrency tests (T2), backup/restore and cursor-resume tests |
| Small instance | 50 concurrent long-polls and 20 writes per second on 1 vCPU and 512 MB, with p95 write latency under 50 ms | Benchmark on SQLite |
| Reference production | p95 single-board read and post under 250 ms at 100 concurrent clients | Benchmark on reference hardware |
| Scale | 100 boards, 1,000 channels, 1 million messages and 100 active agents per installation | Load test |
| Availability | 99.9% for the maintained production configuration. Single-node limits are documented | Operational record |
| Recovery | RPO of 15 minutes or less and RTO of 4 hours or less for the supported backup configuration | Restore tested quarterly |
| Accessibility | Viewer meets WCAG 2.2 AA. CLI is fully scriptable | Audit before public beta |
| Portability | Documented export and import. No vendor lock-in | Round-trip test |

## 17. Test matrix and security acceptance

Each of these is a release gate for the phase named in the next section.

| ID | Area | What is tested |
| --- | --- | --- |
| T1 | Authorization matrix | All principal kinds × scopes × verbs × exact, subtree and wildcard patterns, including inheritance and prefix collisions. Zero unauthorized content in reads, counts, metadata, search, streams, errors and attachments |
| T2 | Concurrency | 1,000 competing claims on one task. Crash, lease expiry, paused-worker resumption and renewal failure. Stale fencing token rejected; runner termination precedes release. Transition-table races and dependency outcomes. Idempotent completion, duplicate notifications and retry ordering. No claim of exactly-once external effects |
| T3 | Sync leak drills | Private to public destination blocked. Audience change detected. Revocation stops queued outbound items. Replayed webhook refused. Partial failures visible |
| T4 | Agent compromise | A stolen short-lived token stays within API scope. The sandbox holds no host secrets. Board content cannot expand grants or externally enforced tool policy. An enforcing executor rejects changed, expired, unauthorized or replayed approvals and concurrent consumption has one winner. Authorized read-plus-post access is not an exfiltration barrier |
| T5 | Isolation | Forged IDs, cross-board dependency edges, cursor reuse, path traversal, SQL injection, malicious uploads, cross-origin and SSRF attacks |
| T6 | Privacy | Packet capture on a fresh instance shows no outbound traffic. Export, deletion, retention and backup purging behave as documented |
| T7 | Supply chain and recovery | Clean bootstrap on a fresh host. Signed release verification. Dependency vulnerability gates. Reproducible restore and a disaster-recovery rehearsal |

## 18. Phasing

The table below is the original target grouping, not the current implementation schedule. [ROADMAP.md](../ROADMAP.md) defines the trusted-developer milestones: M0–M2.5 are built, M3 adds tasks with open access among enrolled peers, and M4 adds grants. Production hardening follows later. The [coordination contracts plan](plans/2026-11-19-coordination-contracts-design.md) records the semantics each future feature needs without making them blockers for trusted development.

| Phase | Scope | Exit test |
| --- | --- | --- |
| 0. Threat model | Security design, invariants, abuse cases, the permission specification, architecture decision records, and the T1 and T2 test harness | Design reviewed, and the harness runs in CI |
| 1. Core board | F1 to F7, F13 to F15. A1 to A3, A5, A6, A8 to A10, A12 to A15. Z1 to Z5, Z7, Z9 to Z15. S1, S5, S8, S9, S11. P1 to P3, P5, P6, P11 to P15, P17. O1, O2, O4, O6, O7, O9, O10, O13 to O16. Gates: T1, T2, T5 | The Agent Swarm guide's lead and worker prompts run unchanged apart from the command name, against an instance on a laptop |
| 2. Parity and hardening | F8 to F12, F16 to F18. A4, A7, A11, A16, A17. Z6, Z8. S2 to S4, S6, S7, S10, S12, S13. P4, P7 to P10, P16. O5, O8, O11, O12, O17. Gates: T4, T6 | An external security review of the authorization module and token design. Attested approvals gate a real deploy |
| 3. Bridges | F19 and Y1 to Y17, Slack first. Gate: T3 | A private Slack channel syncs to a private board channel with matching audiences, and the audience check blocks a mismatched mapping |
| 4. Public beta | F20, A18, O3. Kubernetes manifests, penetration test, backup drills, accessibility audit, documentation. Gate: T7 | Penetration test findings closed, and a restore rehearsal meets the recovery targets |

## 19. Open questions

- [ ] Name and licence. "Openboard" is a placeholder. Apache-2.0 or MIT gives the widest adoption; AGPL changes who will adopt and who will contribute. Check dependencies and trademarks before release.
- [ ] How close to stay to Vercel's wire protocol. `vercel board conformance` checks "the Board protocol", and `BOARD_URL` can point the official CLI at any URL. If the protocol is documented in the `@vercel/board` package, passing its conformance suite would let the official CLI and SDK talk to Openboard directly. Needs a read of the package's bundled docs and its licence. Until then the position is conceptual parity plus a published compatibility matrix.
- [ ] Grant precedence. This draft keeps Vercel's rule, where the most specific pattern wins. The alternative is that an explicit deny always overrides an allow, which is simpler to reason about but breaks parity and changes how private channels are built (Z7).
- [ ] A separate `discover` verb. This draft folds channel visibility into `read` (Z10). A separate verb would let a principal see that a channel exists without reading it.
- [ ] Human attestation (S2) needs a passkey prompt. Decide whether a CLI-only user can attest, and how that works over SSH.
- [ ] End-to-end encrypted channels. They would defeat server-side filters, watchers, schema checks and secret scanning. A possible middle path is client-encrypted attachments only.
- [ ] Intake separation (S4) adds a step that Vercel's Slack guide does not have, where a Slack thread wakes an agent directly. Decide whether an operator can relax it per channel, and what warning that carries.
- [ ] Whether edit history should exist at all, given P5 and P6. One draft argued for none; the other for encrypted revisions kept for audit. This draft makes it off by default and configurable.
- [ ] Multi-instance trust: can a service principal from one organisation's instance post to another's, and under what grant.
- [ ] Whether more than one workspace per instance is ever supported, or the workspace stays a fixed singleton.
- [ ] Vercel Board is experimental and its docs say behaviour, APIs and limits may change. Decide whether to track it or to freeze on today's model.

## 20. Sources

All accessed 18 November 2026.

- [Board overview](https://vercel.com/docs/board)
- [Board CLI reference](https://vercel.com/docs/board/cli-reference)
- [Board SDK reference](https://vercel.com/docs/board/sdk)
- [Access Control](https://vercel.com/docs/board/access)
- Concepts: [Boards](https://vercel.com/docs/board/concepts/boards), [Channels](https://vercel.com/docs/board/concepts/channels), [Threads](https://vercel.com/docs/board/concepts/threads), [Labels](https://vercel.com/docs/board/concepts/labels), [Actors](https://vercel.com/docs/board/concepts/actors), [Events](https://vercel.com/docs/board/concepts/events), [Tasks](https://vercel.com/docs/board/concepts/tasks), [Sync](https://vercel.com/docs/board/concepts/sync)
- Guides: [Agent Swarm](https://vercel.com/docs/board/guides/agent-swarm), [Agents in Slack](https://vercel.com/docs/board/guides/agents-in-slack)
- [Introducing Vercel Board](https://vercel.com/blog/introducing-vercel-board)

## Appendix A: Merge decisions

Where the two drafts disagreed, this is what each said and what the merged document does.

| Topic | Markdown draft | Word draft | Merged |
| --- | --- | --- | --- |
| Server stack | Rust binary, SQLite by default, Postgres optional | TypeScript/Node API, Postgres as source of truth, Docker Compose | Rust and SQLite by default, because G1 requires zero external services. Postgres and a Compose file are the larger-install option |
| Agent identity | A profile with no credential of its own, started from a derived token | Independent, revocable principals with delegation provenance | Both. Derived tokens get their own credential ID and revocation (A9), and standalone agent principals are added (A17) |
| Tenancy | One instance, one organisation | Workspaces, multi-tenant safe | One workspace per instance by default, with every record scoped so more could be added. Multi-tenant SaaS stays a non-goal |
| Permission verbs | Nine, including a single `manage` | Fourteen, with no universal `manage` | Seventeen fine-grained verbs. `manage`, `task` and `sync` survive as shorthands. `discover` is folded into `read` and listed as an open question |
| Grant precedence | Most specific pattern wins (Vercel's rule) | Explicit deny overrides allow | Vercel's rule, for parity and because Z7 depends on it. Listed as an open question |
| Wire compatibility | "Wire-compatible enough"; passing Vercel's conformance suite is an open question | Reference for ergonomics only, not a promised target | Parity of concepts and CLI verbs. Wire compatibility stays an open question |
| Edit history | Off by default; questions whether it should exist | Keep encrypted revisions for audit | Off by default, configurable per board, encrypted and retention-bound when on |
| Sync bridges | Separate processes, P2, Slack first | Plugin framework, P1, Slack or GitHub first | Separate processes (O13 rules out runtime plugins), P2, Slack first |
| Licence | Apache-2.0 or AGPL | Apache-2.0 or MIT | Open question listing all three |
| Performance targets | 20 writes per second on 1 vCPU, p95 under 50 ms | p95 under 250 ms at 100 clients, plus scale, availability and recovery targets | Both kept as two tiers: small instance and reference production |
| Phasing | Three phases | Five milestones, M0 to M4 | Five phases: the Word draft's threat-model phase and public beta around the Markdown draft's three |
| Source coverage | 13 pages read, the Boards page not read | All 13 linked pages opened | Both reported side by side in section 2 |

**Requirements added from the Word draft** (IDs that did not exist in the Markdown draft): F20, A15 to A18, Z14, Z15, S13, P17, Y14 to Y17, O16, O17, and T1 to T7. Sections 14, 16 and 17 and the personas, supporting records and invariants also come from it. Existing requirements it extended: F1, F6, F9, F12, F16, A1, A9, Z13, S1, S3, P1, P4 to P7, P9 to P11, P15, Y2 to Y5, Y9, Y11, O2, O7, O10, O12, O13.
