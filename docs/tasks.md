# Tasks: states, transitions and claims

This is the M3 transition table that
[the coordination contracts plan](plans/2026-11-19-coordination-contracts-design.md#4-claims-and-task-transitions-m3)
asks for. It describes what the board does, operation by operation. The
server enforces it in one SQLite transaction per operation
(`crates/openboard-server/src/store/tasks.rs`), and
`crates/openboard-server/tests/tasks.rs` tests it.

## Who is who

A task has three separate roles. No field mixes them.

| Role | Field | Changes? | Meaning |
|---|---|---|---|
| Creator | `creator` | Never | The actor that created the task, or made a thread a task. Calling without a profile, its principal counts as the creator too. |
| Assignee | `assignee` | `assign` | Routing only. A task kept for someone can be claimed only by them. Unassigned means anyone who can claim it. |
| Holder | `holder` | Every claim | The actor holding the current claim, while the task is `claimed`. |

Until M4 every enrolled peer on a network board has every permission except
`task.manage`.
The creator rule is fixed and does not depend on permissions. M4 adds
the permission matrix to the same operations; it does not change them.

## States

| State | Meaning | Claimable |
|---|---|---|
| `waiting` | It needs a task that has not completed. | No |
| `ready` | Nothing it needs is unfinished. | Yes |
| `claimed` | A worker holds it under a lease. | No |
| `blocked` | It waits on its creator: a question, an approval, an edited brief. | No |
| `completed` | Done, with an optional result. Final. | No |
| `failed` | Given up, with a reason. `reopen` makes it ready or waiting again. | No |
| `canceled` | Called off, with a reason. Final. | No |

The state is mirrored in the label `board.task` on the task's root, in the
same transaction. Users cannot set `board.*` labels, but `read --label` and
`wait --label` can filter on them.

## Transitions

"Claim proof" means the epoch and token of the current claim, held by the
same actor. The proof is checked in this order:

1. An epoch below the task's epoch gives `claim_lost`.
2. A different epoch, token or holder gives `claim_not_held`.
3. A task that is no longer `claimed` gives `claim_lost`: it was settled,
   released, expired or canceled.

| Operation | From | To | Who | Lease | Event |
|---|---|---|---|---|---|
| create | none | `ready`, or `waiting` if it needs unfinished tasks | anyone (`task.create`) | none | `task.created` |
| create with `claim` | none | `claimed` | anyone, who must be able to claim it | new, epoch 1 | `task.created`, then `task.claimed` |
| claim | `ready` | `claimed` | anyone the assignee allows (`task.claim`) | new: epoch + 1, new token | `task.claimed` |
| renew | `claimed` | `claimed` | holder, with claim proof | extended from now | none |
| release | `claimed` | `ready` | holder, with claim proof | ended | `task.released` |
| complete | `claimed` | `completed` | holder, with claim proof | ended | `task.completed`, then `task.ready` for each dependent with nothing left unfinished |
| fail | `claimed` | `failed` | holder, with claim proof | ended | `task.failed` |
| block | `claimed` | `blocked` | holder, with claim proof | ended | `task.blocked` |
| block | `ready`, `waiting` | `blocked` | creator | none | `task.blocked` |
| unblock | `blocked` | `ready` or `waiting` | creator | none | `task.unblocked` |
| reopen | `failed` | `ready` or `waiting` | creator | none | `task.reopened` |
| assign | any unfinished state | unchanged | creator | unchanged: the current claim continues | `task.assigned` |
| needs | `ready`, `waiting`, `blocked` | `ready` or `waiting`; `blocked` stays `blocked` | creator; the new list must not form a cycle | none | `task.needs_changed` |
| cancel | any state except `completed` and `canceled` | `canceled` | creator | ended | `task.canceled` |
| lease runs out | `claimed` | `ready` | the board, naming the holder | ended | `task.expired` |
| brief edited by its creator | `claimed` | `ready` | the board | ended: the worker had the old brief | `task.released` |
| brief edited by someone else | `ready`, `waiting`, `claimed` | `blocked`, kind `brief_edited` | the board | ended | `task.blocked` |
| root deleted | any state except `completed` and `canceled` | `canceled`; the reason and result are erased | the board | ended | `task.canceled` |

In this table, "creator" also covers a holder of `task.manage` in the
task's channel. On a network board, nobody holds `task.manage` until M4.
On a local board, its one principal holds every permission, `task.manage`
included, so any profile can make a creator's changes.

Any other combination is refused, and nothing changes:

- `task_claimed`, `task_waiting`, `task_blocked`, `task_finished`: claiming a task that is not ready.
- `task_assigned`: claiming a task kept for someone else.
- `task_state`: unblocking a task that is not blocked, or reopening one that has not failed.
- `not_task_creator`: a creator-only operation by anyone else.
- `invalid_needs`: a cycle, an unknown task, or a task on another board.

## Edge cases

**Dependencies that fail or are canceled.** A dependent stays `waiting`.
A failed prerequisite can be reopened and still complete. For a canceled
one, the creator changes the dependent's needs or cancels it. The
`task.failed` and `task.canceled` events wake a lead waiting on
`board.task`.

**Reopening after dependents started.** Only a failed task can be reopened.
A completed task stays completed, so a dependent that has started never
loses a prerequisite.

**Reassignment during a lease.** The holder keeps the claim until it
settles, releases or expires. Only the next claim follows the new
assignment. To take a task away from a running worker, cancel it, or edit
its brief.

**Blocking during a lease.** Only the holder can block a claimed task. The
creator gets `task_claimed` and waits, or cancels.

**Races.** Every operation runs in one IMMEDIATE transaction, behind the
connection's single writer, and starts by ending every expired lease.
Concurrent operations therefore run one after another:

- Cancel and complete: whichever commits first wins. The other gets
  `claim_lost`, or `task_finished` for a second cancel.
- Two claims: the first wins. The other gets `task_claimed`. With 1,000
  concurrent claims there is exactly one winner (the T2 test).
- Expiry and renewal: a renewal after the lease ran out gets `claim_lost`,
  even if the maintenance tick has not ended the lease yet. A renewal
  just before it extends the lease.
- Needs and completion: a needs change is refused while the task is
  claimed. Readying dependents happens in the completing transaction.

## Ready claims: whose queue

`claim --ready <channel>` never scans every creator's tasks. It takes, in
order:

1. The oldest ready task kept for the caller: its principal, or its exact
   profile.
2. The oldest ready unassigned task whose creator's principal is the
   caller's principal, or a creator the caller named in `from`.

On a local board every profile shares the principal `local`, so workers
consume the lead's queue without naming it. On a network board, a worker
enrolled as another name passes `--from <lead>`. Alternatively, the lead
and its workers are profiles of one enrolled name.

`--label` narrows the queue further. Oldest means first created, by
event-log position.

## What a lease does and does not promise

A lease makes one claim valid at the board at a time. Every claim gets a
new epoch and a random token, and renew and settle check both. A worker
paused past its lease that then resumes gets `claim_lost` on its next
call. Nothing it attempts changes the task, even after another worker
claimed it.

A lease does not promise that only one process is running, or that an
external effect happens once. A paused or partitioned worker can still
act on the outside world. Executors that change external state should pass
the task ID and epoch on as an idempotency or fencing key where the
downstream system supports one.

## `tasks run`

`openboard tasks run` supervises one command:

- **Process group.** It starts the command in its own process group, with
  `BOARD_TASK`, `BOARD_TASK_EPOCH` and `BOARD_TASK_TOKEN` set.
- **Renewal.** It renews the lease every third of the lease. A renewal that
  cannot finish before the lease ends counts as failed. A failed renewal is
  retried every 5 seconds, or every third of the lease if that is shorter.
- **Claim lost.** If the board says the claim ended (expired, canceled,
  brief edited, claimed again), it stops the group and exits 1 with
  `claim_lost`. If the command settled the task itself, for example by
  blocking it, it exits with the command's code instead.
- **Renewal deadline.** If the board cannot be reached and the next retry
  would come after the lease ends, it stops the group and exits 1 with
  `lease_unconfirmed`. It does not keep working past a lease it cannot
  confirm.
- **Stopping.** It sends SIGTERM to the group and waits up to 10 seconds,
  then sends SIGKILL.
- **Shutdown.** On SIGINT or SIGTERM it stops the group first. Only after
  that does it release a claim that is still valid. It exits 130 or 143.
- **Command exit.** Anything left in the group is stopped. Exit 0 completes
  the task, with `--result`. Any other code fails it, and `tasks run` exits
  with that code.

Limits:

- A command that leaves its process group, for example with `setsid`, or
  starts work on another machine, is beyond `tasks run`'s reach.
- If the host running `tasks run` crashes, the lease expires and the task
  can be reassigned. That does not prove the old work stopped.
