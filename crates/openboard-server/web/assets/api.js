// HTTP access to the board's own API (see crates/openboard-api).
//
// The anti-CSRF token lives only in this module's memory. It is never
// stored, logged or put in a URL, and is fetched again on every page load.

const CSRF_HEADER = "x-openboard-csrf";
/** No request may wait longer than this, so nothing on the page hangs. */
const REQUEST_TIMEOUT_MS = 20000;

let csrfToken = null;
/** ID of the actor whose session this page is showing, or null. */
let shownActorId = null;
let unauthenticatedHandler = () => {};
let identityChangedHandler = () => {};

/** A failed request: the server's `ErrorBody`, or a network failure. */
export class ApiError extends Error {
  constructor(status, error, retryAfter) {
    super((error && error.message) || "The board answered with an error.");
    this.status = status;
    this.code = (error && error.code) || "unknown";
    this.next = (error && error.next) || null;
    this.retryAfter = retryAfter;
  }

  /**
   * True when the same request may succeed later: the board was not
   * reached, asked the caller to slow down, or failed. Anything else is an
   * answer, and asking again would get the same one.
   */
  get transient() {
    return this.status === 0 || this.status === 429 || this.status >= 500;
  }
}

/** Called whenever a request other than sign-in is answered with 401. */
export function onUnauthenticated(handler) {
  unauthenticatedHandler = handler;
}

/**
 * Called with the new `Session` when this browser turns out to be signed
 * in as a different actor than the page was showing, which happens when
 * another tab signs in: the cookie is shared.
 */
export function onIdentityChanged(handler) {
  identityChangedHandler = handler;
}

async function send(method, path, { query, body } = {}) {
  const headers = { accept: "application/json" };
  const timeout = new AbortController();
  const init = {
    method,
    headers,
    cache: "no-store",
    credentials: "same-origin",
    redirect: "error",
    signal: timeout.signal,
  };
  if (body !== undefined) {
    headers["content-type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  if (method !== "GET" && csrfToken) {
    headers[CSRF_HEADER] = csrfToken;
  }
  const timer = setTimeout(() => timeout.abort(), REQUEST_TIMEOUT_MS);
  let response;
  let data = null;
  try {
    response = await fetch(withQuery(path, query), init);
    if (response.status !== 204) {
      data = await response.json().catch(() => null);
    }
  } catch {
    if (timeout.signal.aborted) {
      throw new ApiError(0, { code: "timeout", message: "The board did not answer in time." }, null);
    }
    throw new ApiError(0, { code: "unreachable", message: "The board could not be reached." }, null);
  } finally {
    clearTimeout(timer);
  }
  if (timeout.signal.aborted) {
    throw new ApiError(0, { code: "timeout", message: "The board did not answer in time." }, null);
  }
  if (response.status === 204) {
    return null;
  }
  if (response.ok) {
    return data;
  }
  const retryAfter = Number(response.headers.get("retry-after")) || null;
  throw new ApiError(response.status, data && data.error, retryAfter);
}

function withQuery(path, query) {
  const params = new URLSearchParams();
  for (const [name, value] of Object.entries(query || {})) {
    if (value !== undefined && value !== null) {
      params.set(name, String(value));
    }
  }
  const text = params.toString();
  return text ? `${path}?${text}` : path;
}

/**
 * A request made as the signed-in person. A 401 hands control to the
 * sign-in screen.
 *
 * A write refused for its anti-CSRF token means the session changed under
 * the page. The session is read again, and the write is sent once more
 * only if it still belongs to the actor the page is showing. If it does
 * not, nothing is resent: the person wrote it as someone else.
 */
async function request(method, path, options) {
  try {
    try {
      return await send(method, path, options);
    } catch (error) {
      if (error.code !== "csrf_failed" || method === "GET") {
        throw error;
      }
      if (!(await recheckSession())) {
        throw new ApiError(
          error.status,
          { code: "identity_changed", message: "Nothing was sent, because this browser's sign-in changed." },
          null,
        );
      }
      return await send(method, path, options);
    }
  } catch (error) {
    if (error.status === 401) {
      unauthenticatedHandler();
    }
    throw error;
  }
}

function boardPath(board, rest) {
  return `/v1/boards/${encodeURIComponent(board)}${rest}`;
}

function messagePath(board, messageId, rest = "") {
  return boardPath(board, `/messages/${encodeURIComponent(messageId)}${rest}`);
}

/** What the server is: `{version, commit, api_level, mode}`. Needs no sign-in. */
export async function loadHealth() {
  return send("GET", "/v1/health");
}

// ----- Session ---------------------------------------------------------

/**
 * Who this browser is signed in as. Rejects with status 401 when it is not.
 * Does not call the unauthenticated handler: the caller decides.
 */
export async function loadSession() {
  return adopt(await send("GET", "/v1/web/session"));
}

/** Spends a sign-in code. The server sets the session cookie. */
export async function login(code) {
  return adopt(await send("POST", "/v1/web/login", { body: { code } }));
}

function adopt(session) {
  csrfToken = session.csrf_token || null;
  shownActorId = session.actor.id;
  return session;
}

/**
 * Reads the session again and takes its anti-CSRF token. Resolves to true
 * when it still belongs to the actor the page is showing. Otherwise the
 * matching handler has been called (signed out, or signed in as someone
 * else) and it resolves to false. Rejects when the board cannot say.
 */
export async function recheckSession() {
  const shown = shownActorId;
  let session;
  try {
    session = await send("GET", "/v1/web/session");
  } catch (error) {
    if (error.status !== 401) {
      throw error;
    }
    unauthenticatedHandler();
    return false;
  }
  adopt(session);
  if (shown !== null && session.actor.id !== shown) {
    identityChangedHandler(session);
    return false;
  }
  return true;
}

/** Revokes the session. Goes through `request`, so a stale token cannot stop it. */
export async function logout() {
  await request("POST", "/v1/logout");
  forgetSession();
}

export function forgetSession() {
  csrfToken = null;
  shownActorId = null;
}

// ----- Reads -----------------------------------------------------------

export function listBoards() {
  return request("GET", "/v1/boards");
}

export function listChannels(board) {
  return request("GET", boardPath(board, "/channels"));
}

export function listThreads(board, query) {
  return request("GET", boardPath(board, "/threads"), { query });
}

/** One thread with every message's sanitised HTML. */
export function getThread(board, threadId) {
  return request("GET", boardPath(board, `/threads/${encodeURIComponent(threadId)}`), {
    query: { html: true },
  });
}

/** Who is around on the board: `PresenceList`. */
export function getPresence(board) {
  return request("GET", boardPath(board, "/presence"));
}

/** Address of the board-wide event stream, starting from now. */
export function streamUrl(board) {
  return boardPath(board, "/events/stream");
}

// ----- Writes ----------------------------------------------------------

/** `target` is `{ channel }` to start a thread or `{ thread_id }` to reply. */
export function postMessage(board, target, text, idempotencyKey) {
  return request("POST", boardPath(board, "/messages"), {
    body: { ...target, text, idempotency_key: idempotencyKey },
  });
}

export function editMessage(board, messageId, text, revision) {
  return request("PATCH", messagePath(board, messageId), { body: { text, revision } });
}

export function deleteMessage(board, messageId, revision) {
  return request("DELETE", messagePath(board, messageId), { query: { revision } });
}

export function setLabel(board, messageId, key, value) {
  return request("POST", messagePath(board, messageId, "/labels"), { body: { set: { [key]: value } } });
}

export function clearLabel(board, messageId, key) {
  return request("POST", messagePath(board, messageId, "/labels"), { body: { clear: [key] } });
}

export function addReaction(board, messageId, name) {
  return request("PUT", messagePath(board, messageId, `/reactions/${encodeURIComponent(name)}`));
}

export function removeReaction(board, messageId, name) {
  return request("DELETE", messagePath(board, messageId, `/reactions/${encodeURIComponent(name)}`));
}
