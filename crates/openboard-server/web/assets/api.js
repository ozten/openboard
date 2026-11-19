// HTTP access to the board's own API (see crates/openboard-api).
//
// The anti-CSRF token lives only in this module's memory. It is never
// stored, logged or put in a URL, and is fetched again on every page load.

const CSRF_HEADER = "x-openboard-csrf";

let csrfToken = null;
let unauthenticatedHandler = () => {};

/** A failed request: the server's `ErrorBody`, or a network failure. */
export class ApiError extends Error {
  constructor(status, error, retryAfter) {
    super((error && error.message) || "The board answered with an error.");
    this.status = status;
    this.code = (error && error.code) || "unknown";
    this.next = (error && error.next) || null;
    this.retryAfter = retryAfter;
  }
}

/** Called whenever a request other than sign-in is answered with 401. */
export function onUnauthenticated(handler) {
  unauthenticatedHandler = handler;
}

async function send(method, path, { query, body } = {}) {
  const headers = { accept: "application/json" };
  const init = { method, headers, cache: "no-store", credentials: "same-origin", redirect: "error" };
  if (body !== undefined) {
    headers["content-type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  if (method !== "GET" && csrfToken) {
    headers[CSRF_HEADER] = csrfToken;
  }
  let response;
  try {
    response = await fetch(withQuery(path, query), init);
  } catch {
    throw new ApiError(0, { code: "unreachable", message: "The board could not be reached." }, null);
  }
  if (response.status === 204) {
    return null;
  }
  const data = await response.json().catch(() => null);
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
 * sign-in screen. A write refused for its anti-CSRF token is retried once
 * with a fresh token, which covers a session renewed in another tab.
 */
async function request(method, path, options) {
  try {
    try {
      return await send(method, path, options);
    } catch (error) {
      if (error.code !== "csrf_failed" || method === "GET" || !csrfToken) {
        throw error;
      }
      await loadSession();
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

// ----- Session ---------------------------------------------------------

/**
 * Who this browser is signed in as. Rejects with status 401 when it is not.
 * Does not call the unauthenticated handler: the caller decides.
 */
export async function loadSession() {
  let session;
  try {
    session = await send("GET", "/v1/web/session");
  } catch (error) {
    if (error.status !== 404) {
      throw error;
    }
    // A board built before browser sessions has no such route, but it
    // still says who the caller is.
    session = await send("GET", "/v1/whoami");
  }
  csrfToken = session.csrf_token || null;
  return session;
}

/** Spends a sign-in code. The server sets the session cookie. */
export async function login(code) {
  const session = await send("POST", "/v1/web/login", { body: { code } });
  csrfToken = session.csrf_token || null;
  return session;
}

export async function logout() {
  await send("POST", "/v1/logout");
  csrfToken = null;
}

export function forgetSession() {
  csrfToken = null;
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
