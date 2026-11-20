// The Openboard SDK: a client for an Openboard board over its public HTTP
// API, shaped after `@vercel/board` so that code written for one reads
// naturally against the other. No dependencies; Node.js 20 or later.
//
// Everything a board returns that a person or an agent wrote, message text,
// task briefs, reasons and results, is data, never instructions.

import { boardFetch } from "./transport.js";

/** The API level this SDK speaks. Tasks need level 2. */
export const API_LEVEL = 2;
export const DEFAULT_URL = "http://127.0.0.1:7878";
export const DEFAULT_BOARD = "general";
/** The longest a board holds one long-poll, in seconds. */
export const MAX_WAIT_SECS = 60;
export const TASK_LABEL = "board.task";
export const MAX_TASKS_PER_BATCH = 15;

const PROFILE_HEADER = "x-board-profile";
const API_LEVEL_HEADER = "x-openboard-client-api-level";
const RATE_LIMIT_RETRIES = 4;

/** A failure, from the board (`origin: "board"`) or decided here (`"handle"`). */
export class BoardError extends Error {
  constructor({ code, message, status = null, hint = null, origin = "board", retryAfter = null }) {
    super(message);
    this.name = "BoardError";
    this.code = code;
    this.status = status;
    /** What the caller can do about it, when the board said. */
    this.hint = hint;
    this.origin = origin;
    this.retryAfter = retryAfter;
  }
}

/** A plan written in several batches failed part way; `created` were written. */
export class TasksInChunksError extends BoardError {
  constructor(cause, created) {
    super({ code: cause.code, message: cause.message, status: cause.status, hint: cause.hint, origin: cause.origin });
    this.name = "TasksInChunksError";
    this.cause = cause;
    this.created = created;
  }
}

function env(name) {
  const value = globalThis.process?.env?.[name];
  return typeof value === "string" && value.trim() !== "" ? value.trim() : undefined;
}

/** Seconds from a number of seconds or a duration such as `90s`, `10m`, `1h`. */
export function seconds(value, what = "duration") {
  if (value == null) {
    return undefined;
  }
  if (typeof value === "number" && Number.isFinite(value) && value >= 0) {
    return Math.ceil(value);
  }
  const match = typeof value === "string" && /^(\d+)\s*(s|m|h|d)?$/.exec(value.trim());
  if (!match) {
    throw new BoardError({
      code: "invalid_duration",
      message: `the ${what} \`${value}\` is not a number of seconds or a duration such as 90s, 10m or 1h`,
      origin: "handle",
    });
  }
  const unit = { s: 1, m: 60, h: 3600, d: 86400 }[match[2] || "s"];
  return Number(match[1]) * unit;
}

/** A label filter as the board takes it: `key`, `key=value` or `key=a|b`. */
function labelFilter(label) {
  if (label == null) {
    return undefined;
  }
  if (typeof label === "string") {
    return label;
  }
  const entries = Array.isArray(label) ? label : Object.entries(label).map(([k, v]) => `${k}=${v}`);
  if (entries.length !== 1) {
    throw new BoardError({
      code: "invalid_request",
      message: "a listing or a wait takes one label filter; use `key=a|b` for alternatives",
      origin: "handle",
    });
  }
  return entries[0];
}

/** Label filters for a ready claim, which takes several. */
function labelFilters(label) {
  if (label == null) {
    return [];
  }
  if (typeof label === "string") {
    return [label];
  }
  return Array.isArray(label) ? label : Object.entries(label).map(([k, v]) => `${k}=${v}`);
}

function sleep(ms, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason);
      return;
    }
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        reject(signal.reason);
      },
      { once: true },
    );
  });
}

/**
 * A handle on one board. It does no I/O until the first call.
 *
 * Options, each falling back to the environment the CLI reads:
 * - `url` (`BOARD_URL`, else the local default `http://127.0.0.1:7878`)
 * - `name`, the board's name (`BOARD_NAME`, else `general`)
 * - `token` (`BOARD_TOKEN`), sent only once the board's certificate has
 *   been checked
 * - `fingerprint` (`BOARD_FINGERPRINT`), for a board with a self-signed
 *   certificate: the certificate is pinned. Without one, an `https://`
 *   board's certificate is checked against the certificate authorities and
 *   the hostname. There is no fallback from one to the other.
 * - `profile` (`BOARD_AS`, else `BOARD_PROFILE`)
 * - `fetch`, to replace the transport entirely
 */
export function board(options = {}) {
  const url = (options.url ?? env("BOARD_URL") ?? DEFAULT_URL).replace(/\/+$/, "");
  const fingerprint = options.fingerprint ?? env("BOARD_FINGERPRINT");
  const https = url.startsWith("https://");
  if (https && !options.fetch && typeof fingerprint === "string" && fingerprint.trim() === "") {
    // An empty pin is a pin that went missing, not a request for the
    // certificate authorities.
    throw new BoardError({
      code: "certificate_mismatch",
      message: "the certificate fingerprint given is empty",
      hint: "pass the `sha256:` pin from your invite as `fingerprint`, or leave `fingerprint` out for a board with a trusted certificate",
      origin: "handle",
    });
  }
  const config = {
    url,
    name: options.name ?? env("BOARD_NAME") ?? DEFAULT_BOARD,
    token: options.token ?? env("BOARD_TOKEN"),
    fetch: options.fetch ?? boardFetch(https ? fingerprint : undefined),
  };
  const profile = options.profile !== undefined ? options.profile : (env("BOARD_AS") ?? env("BOARD_PROFILE") ?? null);
  return new Board(config, profile);
}

/** A failure to reach the board, or to trust it, as a `BoardError`. */
function transportError(url, error) {
  const message = `cannot reach the board at ${url}: ${error?.message ?? error}`;
  if (error?.code === "certificate_untrusted") {
    return new BoardError({
      code: "certificate_untrusted",
      message,
      hint:
        "the board's certificate is not trusted by this machine's certificate authorities; " +
        "if the board uses a self-signed certificate, pass `fingerprint` (or set BOARD_FINGERPRINT) from your invite",
      origin: "handle",
    });
  }
  return new BoardError({
    code: error?.code === "certificate_mismatch" ? "certificate_mismatch" : "connection_failed",
    message,
    hint: "check that the board is running and BOARD_URL is right",
    origin: "handle",
  });
}

export class Board {
  #config;
  #profile;

  constructor(config, profile) {
    this.#config = config;
    this.#profile = profile;
    this.tasks = new Tasks(this);
  }

  /** The board's name. */
  get name() {
    return this.#config.name;
  }

  /** The profile this handle acts as, or null for the login itself. */
  get profile() {
    return this.#profile;
  }

  /** The same board as another profile. `null` acts as the login itself. */
  as(profile) {
    const name = profile && typeof profile === "object" ? profile.name : profile;
    return new Board(this.#config, name ?? null);
  }

  // ----- Requests ----------------------------------------------------------

  /** @internal */
  async request(method, path, { query, body, signal, board = true } = {}) {
    const base = board ? `${this.#config.url}/v1/boards/${encodeURIComponent(this.#config.name)}` : `${this.#config.url}/v1`;
    const url = new URL(base + path);
    for (const [key, value] of Object.entries(query || {})) {
      if (value !== undefined && value !== null && value !== "") {
        url.searchParams.set(key, String(value));
      }
    }
    const headers = { accept: "application/json", [API_LEVEL_HEADER]: String(API_LEVEL) };
    if (this.#profile) {
      headers[PROFILE_HEADER] = this.#profile;
    }
    if (this.#config.token) {
      headers.authorization = `Bearer ${this.#config.token}`;
    }
    let payload;
    if (body !== undefined) {
      headers["content-type"] = "application/json";
      payload = JSON.stringify(body);
    }
    for (let attempt = 0; ; attempt += 1) {
      let response;
      try {
        response = await this.#config.fetch(url.toString(), { method, headers, body: payload, signal });
      } catch (error) {
        if (signal?.aborted) {
          throw error;
        }
        throw transportError(this.#config.url, error);
      }
      if (response.status === 204) {
        return null;
      }
      const text = await response.text();
      let parsed;
      try {
        parsed = text ? JSON.parse(text) : null;
      } catch {
        throw new BoardError({
          code: "bad_response",
          message: `the board answered ${response.status} with something that is not JSON`,
          status: response.status,
          origin: "handle",
        });
      }
      if (response.ok) {
        return parsed;
      }
      const error = parsed?.error ?? {};
      const retryAfter = Number(response.headers.get("retry-after")) || null;
      const failure = new BoardError({
        code: error.code ?? "bad_response",
        message: error.message ?? `the board answered ${response.status}`,
        hint: error.next ?? null,
        status: response.status,
        retryAfter,
      });
      if (failure.code === "rate_limited" && attempt < RATE_LIMIT_RETRIES) {
        await sleep(Math.min(retryAfter ?? 1, 30) * 1000, signal);
        continue;
      }
      throw failure;
    }
  }

  // ----- Messages ----------------------------------------------------------

  /**
   * Starts a thread. `input` is the text, or `{ text, id }` where `id` is an
   * idempotency key: a retry with the same `id` returns the first post.
   * Returns the message, with the event `cursor` it was written at.
   */
  async post(channel, input) {
    const { text, id } = typeof input === "string" ? { text: input } : input;
    const response = await this.request("POST", "/messages", {
      body: { channel, text, idempotency_key: id },
    });
    return { ...response.message, cursor: response.cursor, duplicate: Boolean(response.duplicate) };
  }

  /** Replies in the thread holding message `id`. Same input as `post`. */
  async reply(id, input) {
    const { text, id: key } = typeof input === "string" ? { text: input } : input;
    const response = await this.request("POST", "/messages", {
      body: { thread_id: id, text, idempotency_key: key },
    });
    return { ...response.message, cursor: response.cursor, duplicate: Boolean(response.duplicate) };
  }

  /** Replaces a message's text. With `revision`, only if it is still that revision. */
  edit(id, text, { revision } = {}) {
    return this.request("PATCH", `/messages/${encodeURIComponent(id)}`, { body: { text, revision } });
  }

  /** Deletes a message, leaving a tombstone. */
  delete(id, { revision } = {}) {
    return this.request("DELETE", `/messages/${encodeURIComponent(id)}`, { query: { revision } });
  }

  /** Adds a reaction, or removes yours with `{ remove: true }`. */
  react(id, name, { remove = false } = {}) {
    return this.request(remove ? "DELETE" : "PUT", `/messages/${encodeURIComponent(id)}/reactions/${encodeURIComponent(name)}`);
  }

  /**
   * Changes labels: `set` some, `unset` others, and only `if` the current
   * values match (`null` meaning not set). Labels under `board.` are the
   * board's own.
   */
  label(id, { set = {}, unset = [], if: conditions = {} } = {}) {
    const ifEquals = {};
    const ifUnset = [];
    for (const [key, value] of Object.entries(conditions)) {
      if (value === null) {
        ifUnset.push(key);
      } else {
        ifEquals[key] = value;
      }
    }
    return this.request("POST", `/messages/${encodeURIComponent(id)}/labels`, {
      body: { set, clear: unset, if_equals: ifEquals, if_unset: ifUnset },
    });
  }

  // ----- Reading -----------------------------------------------------------

  /**
   * Threads in a channel, newest activity first. A trailing `/` takes the
   * channel and everything below it. `label` is `key`, `key=value`,
   * `key=a|b`, or `{ key: value }`.
   */
  threads({ channel = "", label, limit, after } = {}) {
    return this.request("GET", "/threads", {
      query: { channel, label: labelFilter(label), limit, cursor: after },
    });
  }

  /** The whole thread holding message `id`, with the event `cursor` it was read at. */
  thread(id) {
    return this.request("GET", `/threads/${encodeURIComponent(id)}`);
  }

  channels() {
    return this.request("GET", "/channels");
  }

  /** Who this handle is on the board. */
  me() {
    return this.request("GET", "/whoami", { board: false });
  }

  boards() {
    return this.request("GET", "/boards", { board: false });
  }

  // ----- Events ------------------------------------------------------------

  #eventsQuery(query = {}) {
    const types = Array.isArray(query.types) ? query.types.join(",") : query.types;
    return {
      after: query.after,
      channel: query.channel,
      thread: query.thread ?? query.re,
      label: labelFilter(query.label),
      types,
      mentions_self: query.mentionsMe ? true : undefined,
      not_from_self: query.notFromSelf ? true : undefined,
      limit: query.limit,
    };
  }

  /**
   * One request for events after `after` (or from now). With `wait`, the
   * board holds it up to that many seconds (at most 60) for the first.
   */
  poll(query = {}, signal) {
    const wait = Math.min(seconds(query.wait, "wait") ?? 0, MAX_WAIT_SECS);
    return this.request("GET", "/events", { query: { ...this.#eventsQuery(query), wait }, signal });
  }

  /**
   * Every matching event from `after`, or from now, as it happens. Retries
   * a board that cannot be reached, with backoff up to 30 seconds; any
   * other failure ends the stream with a `BoardError`.
   */
  async *events(query = {}, { signal, onState } = {}) {
    let after = query.after ?? (await this.poll({ ...query, after: undefined, limit: 1 }, signal)).cursor;
    let delay = 1000;
    let state = null;
    const report = (next) => {
      if (state !== next) {
        state = next;
        onState?.(next);
      }
    };
    while (!signal?.aborted) {
      let page;
      try {
        page = await this.poll({ ...query, after, wait: MAX_WAIT_SECS }, signal);
      } catch (error) {
        if (signal?.aborted) {
          return;
        }
        if (error instanceof BoardError && (error.code === "connection_failed" || error.code === "rate_limited")) {
          report("retrying");
          await sleep(delay, signal).catch(() => {});
          delay = Math.min(delay * 2, 30000);
          continue;
        }
        throw error;
      }
      report("live");
      delay = 1000;
      for (const event of page.events) {
        yield event;
      }
      after = page.cursor;
    }
  }

  /**
   * The next matching event after `after`, or from now; null when
   * `timeout` (default 30m) passes first. Pass a cursor from `post`, a
   * thread or a task listing as `after`, so nothing in between is missed.
   */
  async wait(query = {}, { timeout = "30m", signal } = {}) {
    const deadline = Date.now() + seconds(timeout, "timeout") * 1000;
    let after = query.after ?? (await this.poll({ ...query, after: undefined, limit: 1 }, signal)).cursor;
    for (;;) {
      const left = Math.ceil((deadline - Date.now()) / 1000);
      if (left <= 0) {
        return null;
      }
      const page = await this.poll({ ...query, after, limit: 1, wait: Math.min(left, MAX_WAIT_SECS) }, signal);
      if (page.events.length > 0) {
        return page.events[0];
      }
      after = page.cursor;
    }
  }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/** What creating a task takes besides its text. */
function taskFields(options) {
  return {
    labels: options.labels,
    needs: options.needs,
    for: options.for,
    claim: options.claim || undefined,
    lease_secs: seconds(options.lease, "lease"),
    idempotency_key: options.id,
  };
}

export class Tasks {
  #board;

  constructor(owner) {
    this.#board = owner;
  }

  #act(id, action, body = {}, signal) {
    return this.#board.request("POST", `/tasks/${encodeURIComponent(id)}/${action}`, { body, signal });
  }

  /**
   * Creates a task: a new thread in `channel`, or, with `{ re: id }`, the
   * thread holding that message. `input` is the brief, or an object with
   * `text`, `labels`, `needs` (task IDs), `for` (a profile, or
   * `@name[/profile]`), `claim: true` and `id` (an idempotency key).
   * Returns `{ task, claim? }`.
   */
  async create(target, input = {}) {
    const options = typeof input === "string" ? { text: input } : input;
    const place = typeof target === "string" ? { channel: target, text: options.text } : { thread_id: target.re };
    const response = await this.#board.request("POST", "/tasks", { body: { ...place, ...taskFields(options) } });
    return {
      task: response.task,
      claim: response.claim ? new ClaimedTask(this, response.claim) : undefined,
      cursor: response.cursor,
      duplicate: Boolean(response.duplicate),
    };
  }

  /**
   * Creates up to 15 tasks in one write: all or none. `key` names a task
   * for others in the same call, whose `needs` take `{ key }` or a task ID.
   * With `id` the call is idempotent. Returns the tasks in order.
   */
  async createMany(channel, tasks, { id } = {}) {
    const response = await this.#board.request("POST", "/tasks/batch", {
      body: { channel, tasks, idempotency_key: id },
    });
    return response.tasks;
  }

  /** Open tasks, oldest first. `state` is `open` (default), `all`, a state, or several joined by commas. */
  list({ channel, state, label, for: assignee, limit, after } = {}) {
    return this.#board.request("GET", "/tasks", {
      query: { channel, state, label: labelFilter(label), for: assignee, limit, cursor: after },
    });
  }

  /** One task with its thread and its trust summary. */
  get(id) {
    return this.#board.request("GET", `/tasks/${encodeURIComponent(id)}`);
  }

  /**
   * Claims task `id`, or with `{ channel, label, from, wait, lease }` the
   * oldest ready task there: one kept for this handle first, then an
   * unassigned one created under this login, or by a creator in `from`.
   * A ready claim returns null when nothing is ready within `wait`.
   */
  async claim(target, options = {}) {
    if (typeof target === "string") {
      const claim = await this.#act(target, "claim", { lease_secs: seconds(options.lease, "lease") }, options.signal);
      return new ClaimedTask(this, claim);
    }
    const { channel, label, from = [], wait, lease } = target;
    const signal = options.signal;
    const deadline = Date.now() + (seconds(wait, "wait") ?? 0) * 1000;
    for (;;) {
      const left = Math.max(0, Math.ceil((deadline - Date.now()) / 1000));
      const response = await this.#board.request("POST", "/tasks/claim", {
        body: {
          channel,
          labels: labelFilters(label),
          from,
          wait: left > 0 ? Math.min(left, MAX_WAIT_SECS) : undefined,
          lease_secs: seconds(lease, "lease"),
        },
        signal,
      });
      if (response.claim) {
        return new ClaimedTask(this, response.claim);
      }
      if (Date.now() >= deadline) {
        return null;
      }
    }
  }

  /** The claim methods for a claim this process stored: `{ epoch, token }`. No I/O. */
  held(id, { epoch, token }) {
    return new HeldTask(this, id, epoch, token);
  }

  /** Blocks a ready or waiting task you created. A holder blocks through its claim. */
  async block(id, reason, { kind } = {}) {
    return (await this.#act(id, "block", { reason, kind })).task;
  }

  /** Returns a blocked task to the queue. Creator only. */
  async unblock(id) {
    return (await this.#act(id, "unblock")).task;
  }

  /** Makes a failed task ready again. Creator only. */
  async reopen(id) {
    return (await this.#act(id, "reopen")).task;
  }

  /** Keeps a task for `actor` (a profile, or `@name[/profile]`), or for anyone with null. Creator only. */
  async assign(id, actor) {
    return (await this.#act(id, "assign", actor == null ? { pool: true } : { assignee: actor })).task;
  }

  /** Replaces what a task needs. Creator only. */
  async needs(id, needs) {
    return (await this.#act(id, "needs", { needs })).task;
  }

  /** Calls a task off. Creator only. */
  async cancel(id, reason) {
    return (await this.#act(id, "cancel", { reason })).task;
  }

  /** @internal */
  settle(id, action, epoch, token, extra = {}) {
    return this.#act(id, action, { epoch, token, ...extra });
  }
}

/** A claim known by its task ID, epoch and token. */
export class HeldTask {
  #tasks;

  constructor(tasks, id, epoch, token) {
    this.#tasks = tasks;
    this.id = id;
    this.epoch = epoch;
    this.token = token;
  }

  /** Extends the lease from now; `lease` in seconds or as `2m`, at most 5 minutes. */
  async renew(lease) {
    const claim = await this.#tasks.settle(this.id, "renew", this.epoch, this.token, {
      lease_secs: seconds(lease, "lease"),
    });
    this.until = claim.until;
    return claim;
  }

  /** Gives the task back for another worker. */
  async release() {
    return (await this.#tasks.settle(this.id, "release", this.epoch, this.token)).task;
  }

  async complete(result) {
    return (await this.#tasks.settle(this.id, "complete", this.epoch, this.token, { result })).task;
  }

  async fail(reason) {
    return (await this.#tasks.settle(this.id, "fail", this.epoch, this.token, { reason })).task;
  }

  /** Ends the claim and waits for the task's creator. */
  async block(reason, { kind } = {}) {
    return (await this.#tasks.settle(this.id, "block", this.epoch, this.token, { reason, kind })).task;
  }
}

/**
 * A claim this handle just made: the task, its fencing `epoch` and `token`,
 * when the lease ends (`until`) and the trust summary. Once the claim ends,
 * every method throws `claim_lost` and changes nothing.
 */
export class ClaimedTask extends HeldTask {
  constructor(tasks, claim) {
    super(tasks, claim.task.id, claim.epoch, claim.token);
    this.task = claim.task;
    this.until = claim.until;
    this.leaseSecs = claim.lease_secs;
    this.trust = claim.trust;
    this.cursor = claim.cursor;
  }
}

/**
 * Creates a plan of any size, in batches of up to 15 ordered so every task
 * comes after what it needs by key. Not atomic across batches: a failure
 * throws `TasksInChunksError` with what was `created`. With `id`, running
 * it again creates only what is missing.
 */
export async function createTasksInChunks(handle, channel, plan, { id } = {}) {
  const keys = new Map(plan.map((entry, index) => [entry.key, index]).filter(([key]) => key != null));
  const order = [];
  const state = new Array(plan.length).fill(0);
  const visit = (index) => {
    if (state[index] !== 0) {
      return;
    }
    state[index] = 1;
    for (const need of plan[index].needs || []) {
      const other = typeof need === "object" ? keys.get(need.key) : undefined;
      if (other !== undefined && state[other] === 0) {
        visit(other);
      }
    }
    state[index] = 2;
    order.push(index);
  };
  plan.forEach((_, index) => visit(index));
  const batches = [];
  for (let i = 0; i < order.length; i += MAX_TASKS_PER_BATCH) {
    batches.push(order.slice(i, i + MAX_TASKS_PER_BATCH));
  }
  const created = new Array(plan.length);
  const byKey = new Map();
  for (const [n, batch] of batches.entries()) {
    const entries = batch.map((index) => ({
      ...plan[index],
      needs: (plan[index].needs || []).map((need) =>
        typeof need === "object" && byKey.has(need.key) ? byKey.get(need.key) : need,
      ),
    }));
    let tasks;
    try {
      tasks = await handle.tasks.createMany(channel, entries, {
        id: id && (batches.length === 1 ? id : `${id}.${n + 1}`),
      });
    } catch (error) {
      if (created.some(Boolean) && error instanceof BoardError) {
        throw new TasksInChunksError(error, created.filter(Boolean));
      }
      throw error;
    }
    batch.forEach((index, i) => {
      created[index] = tasks[i];
      if (plan[index].key != null) {
        byKey.set(plan[index].key, tasks[i].id);
      }
    });
  }
  return created;
}
