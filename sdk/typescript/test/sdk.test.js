// End-to-end tests: a real board, started from the workspace's built
// `openboard` binary, driven through the SDK. Build it first with
// `cargo build -p openboard-cli`, or set OPENBOARD_BIN.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";

import { BoardError, board, createTasksInChunks, seconds } from "../src/index.js";
import { parseFingerprint } from "../src/transport.js";

const BIN =
  process.env.OPENBOARD_BIN ?? fileURLToPath(new URL("../../../target/debug/openboard", import.meta.url));

let server;
let dir;
let url;

before(async () => {
  dir = mkdtempSync(join(tmpdir(), "openboard-sdk-"));
  server = spawn(BIN, ["--json", "serve", "--port", "0", "--data-dir", join(dir, "data")], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  const first = await new Promise((resolve, reject) => {
    const lines = createInterface({ input: server.stdout });
    lines.once("line", resolve);
    server.once("error", reject);
    server.once("exit", (code) => reject(new Error(`serve exited with ${code}`)));
  });
  url = JSON.parse(first).url;
});

after(() => {
  server?.kill("SIGTERM");
  rmSync(dir, { recursive: true, force: true });
});

const as = (profile) => board({ url, profile, token: undefined, fingerprint: undefined });

test("posts, replies, labels and reads", async () => {
  const lead = as("lead");
  const root = await lead.post("notes", { text: "first", id: "k1" });
  assert.equal(root.channel, "notes");
  assert.ok(root.cursor);
  const again = await lead.post("notes", { text: "first", id: "k1" });
  assert.equal(again.id, root.id);
  assert.equal(again.duplicate, true);
  await as("helper").reply(root.id, "second");
  await lead.label(root.id, { set: { status: "open" }, if: { status: null } });
  await assert.rejects(lead.label(root.id, { set: { status: "x" }, if: { status: null } }), (error) => {
    assert.ok(error instanceof BoardError);
    assert.equal(error.code, "label_conflict");
    return true;
  });
  const thread = await lead.thread(root.id);
  assert.equal(thread.replies.length, 1);
  assert.equal(thread.root.labels.status, "open");
  const list = await lead.threads({ channel: "notes", label: { status: "open" } });
  assert.equal(list.threads.length, 1);
  assert.equal((await lead.me()).actor.handle, "local/lead");
});

test("a lead plans, workers claim and settle, the lead waits on task state", async () => {
  const lead = as("lead");
  const worker = as("worker-1");
  const [fix, review] = await lead.tasks.createMany(
    "build",
    [
      { key: "fix", text: "fix the parser", labels: { role: "worker" } },
      { key: "review", text: "review the fix", needs: [{ key: "fix" }] },
    ],
    { id: "plan-1" },
  );
  assert.equal(fix.task.state, "ready");
  assert.equal(review.task.state, "waiting");
  const listed = await lead.tasks.list({ channel: "build" });

  const waiting = lead.wait(
    { channel: "build", label: "board.task=completed|blocked|failed", after: listed.cursor },
    { timeout: "30s" },
  );
  const claim = await worker.tasks.claim({ channel: "build", label: { role: "worker" }, wait: 5 });
  assert.ok(claim);
  assert.equal(claim.task.id, fix.id);
  assert.equal(claim.trust.creator.handle, "local/lead");
  await claim.renew("1m");
  // Another profile cannot use the claim.
  await assert.rejects(as("worker-2").tasks.held(fix.id, claim).complete(), { code: "claim_not_held" });
  const done = await claim.complete("tests pass");
  assert.equal(done.task.state, "completed");
  const event = await waiting;
  assert.equal(event.type, "task.completed");
  assert.equal(event.message_id, fix.id);
  // Its dependent is ready now, and a late call on the ended claim changes nothing.
  assert.equal((await lead.tasks.get(review.id)).task.task.state, "ready");
  await assert.rejects(claim.release(), { code: "claim_lost" });

  // Nothing else is ready for this label.
  assert.equal(await worker.tasks.claim({ channel: "build", label: { role: "worker" } }), null);
});

test("creators block, unblock, assign and cancel; workers block their claim", async () => {
  const lead = as("lead");
  const { task } = await lead.tasks.create("ops", { text: "pick a database" });
  const claim = await as("worker-1").tasks.claim(task.id, { lease: 30 });
  assert.equal(claim.leaseSecs, 30);
  const blocked = await claim.block("Postgres or SQLite?", { kind: "needs_input" });
  assert.equal(blocked.task.block_kind, "needs_input");
  assert.equal((await lead.tasks.unblock(task.id)).task.state, "ready");
  // On a local board its one principal holds every permission, task.manage
  // included, so the creator-only rule is tested on network boards.
  assert.equal((await lead.tasks.assign(task.id, "worker-2")).task.assignee, "local/worker-2");
  await assert.rejects(as("worker-1").tasks.claim(task.id), { code: "task_assigned" });
  assert.equal((await lead.tasks.cancel(task.id, "not needed")).task.state, "canceled");

  const { task: mine, claim: held } = await lead.tasks.create("ops", { text: "do it now", claim: true });
  assert.equal(held.task.id, mine.id);
  assert.equal((await held.fail("could not")).task.state, "failed");
  assert.equal((await lead.tasks.reopen(mine.id)).task.state, "ready");
});

test("a long plan is written in ordered batches, once per id", async () => {
  const lead = as("lead");
  const plan = Array.from({ length: 20 }, (_, n) => ({
    key: `t${19 - n}`,
    text: `step ${19 - n}`,
    needs: n < 19 ? [{ key: `t${18 - n}` }] : [],
  }));
  const created = await createTasksInChunks(lead, "plan", plan, { id: "big" });
  assert.equal(created.length, 20);
  assert.equal(created[19].task.state, "ready");
  assert.ok(created.slice(0, 19).every((t) => t.task.state === "waiting"));
  const again = await createTasksInChunks(lead, "plan", plan, { id: "big" });
  assert.deepEqual(again.map((t) => t.id), created.map((t) => t.id));
});

test("events stream from a cursor", async () => {
  const lead = as("lead");
  const first = await lead.post("stream", "one");
  const controller = new AbortController();
  const seen = [];
  const reading = (async () => {
    for await (const event of lead.events({ channel: "stream", after: first.cursor }, { signal: controller.signal })) {
      seen.push(event);
      if (seen.length === 2) {
        controller.abort();
      }
    }
  })();
  await lead.post("stream", "two");
  await lead.post("stream", "three");
  await reading;
  assert.deepEqual(seen.map((e) => e.type), ["message.posted", "message.posted"]);
});

test("presence lists the seats that have been seen", async () => {
  await as("scout").post("presence", "here");
  const roster = await as("scout").presence();
  assert.equal(roster.windows.live_secs, 120);
  const local = roster.principals.find((principal) => principal.handle === "local");
  assert.ok(local);
  assert.equal(local.state, "live");
  const scout = local.seats.find((seat) => seat.handle === "local/scout");
  assert.equal(scout?.state, "live");
  assert.deepEqual(scout?.listening, []);
});

test("helpers", () => {
  assert.equal(seconds("10m"), 600);
  assert.equal(seconds(5), 5);
  assert.throws(() => seconds("soon"), { code: "invalid_duration" });
  assert.equal(parseFingerprint(`sha256:${"A".repeat(64)}`), "a".repeat(64));
  assert.equal(parseFingerprint("sha1:00"), null);
  assert.throws(() => board({ url: "https://board.example:7878", fingerprint: "" }), { code: "certificate_mismatch" });
});

test("a network board is reached over pinned TLS with a token", async () => {
  const netDir = mkdtempSync(join(tmpdir(), "openboard-sdk-net-"));
  const net = spawn(BIN, ["--json", "serve", "--network", "--open", "--port", "0", "--data-dir", join(netDir, "data")], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  try {
    const started = JSON.parse(
      await new Promise((resolve, reject) => {
        createInterface({ input: net.stdout }).once("line", resolve);
        net.once("exit", (code) => reject(new Error(`serve exited with ${code}`)));
      }),
    );
    const run = (args, extraEnv = {}) =>
      new Promise((resolve, reject) => {
        const child = spawn(BIN, args, {
          env: { ...process.env, OPENBOARD_CONFIG_DIR: join(netDir, "config"), ...extraEnv },
          stdio: ["ignore", "pipe", "inherit"],
        });
        let out = "";
        child.stdout.on("data", (chunk) => (out += chunk));
        child.on("exit", (code) => (code === 0 ? resolve(out) : reject(new Error(`exit ${code}: ${out}`))));
      });
    const { invite } = JSON.parse(await run(["--json", "invite", "sdk-agent", "--data-dir", join(netDir, "data")]));
    await run(["--json", "join", invite]);
    const { readFileSync } = await import("node:fs");
    const login = JSON.parse(readFileSync(join(netDir, "config", "logins.json"), "utf8")).logins[0];

    const lead = board({ url: started.url, fingerprint: started.fingerprint, token: login.token, profile: "lead" });
    assert.equal((await lead.me()).actor.handle, "sdk-agent/lead");
    const { task } = await lead.tasks.create("build", "only the creator cancels");
    await assert.rejects(lead.as("worker").tasks.cancel(task.id, "no"), { code: "not_task_creator" });
    assert.equal((await lead.tasks.cancel(task.id, "yes")).task.state, "canceled");

    // Another certificate's pin is refused before anything is sent.
    const wrong = board({ url: started.url, fingerprint: `sha256:${"0".repeat(64)}`, token: login.token });
    await assert.rejects(wrong.me(), { code: "certificate_mismatch" });
    // No token: the board refuses.
    await assert.rejects(board({ url: started.url, fingerprint: started.fingerprint, token: "" }).me(), {
      code: "unauthenticated",
    });
  } finally {
    net.kill("SIGTERM");
    rmSync(netDir, { recursive: true, force: true });
  }
});
