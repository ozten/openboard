// The Openboard web UI: sign-in, the board, channel and thread lists, the
// address bar, and the one live event stream. The thread pane is thread.js.

import * as api from "./api.js";
import { h, keepFocus, kindBadge, refreshTimes, timeElement } from "./dom.js";
import { bindSendForm, clearError, showError } from "./forms.js";
import { createThreadView } from "./thread.js";

const DEFAULT_BOARD = "general";
const WEB_CODE_PREFIX = "obw1_";
const THREADS_PER_PAGE = 50;
const MAX_THREADS_PER_PAGE = 500;
const SNIPPET_CHARS = 200;
const LABELS_IN_LIST = 3;
/** Events arriving within this time are answered by one round of reads. */
const REFRESH_DELAY_MS = 300;
const RETRY_DELAY_MS = 5000;
const SESSION_CHECK_INTERVAL_MS = 10000;
const MAX_REOPEN_DELAY_MS = 30000;
/** At this width all three panes show at once; below it, one at a time. */
const wideLayout = window.matchMedia("(min-width: 900px)");

const byId = (id) => document.getElementById(id);
const el = {
  gate: byId("gate"),
  gateLoading: byId("gate-loading"),
  gateBody: byId("gate-body"),
  gateText: byId("gate-text"),
  gateHint: byId("gate-hint"),
  gateError: byId("gate-error"),
  gateSignIn: byId("gate-sign-in"),
  gateContinue: byId("gate-continue"),
  gateRetry: byId("gate-retry"),
  app: byId("app"),
  boardSelect: byId("board-select"),
  liveStatus: byId("live-status"),
  meKind: byId("me-kind"),
  meHandle: byId("me-handle"),
  signOut: byId("sign-out"),
  notice: byId("notice"),
  panes: byId("panes"),
  channelsTitle: byId("channels-title"),
  newChannel: byId("new-channel"),
  channelList: byId("channel-list"),
  channelsEmpty: byId("channels-empty"),
  backToChannels: byId("back-to-channels"),
  threadsTitle: byId("threads-title"),
  newThread: byId("new-thread"),
  newThreadForm: byId("new-thread-form"),
  newThreadChannel: byId("new-thread-channel"),
  newThreadText: byId("new-thread-text"),
  newThreadError: byId("new-thread-error"),
  newThreadCancel: byId("new-thread-cancel"),
  threadList: byId("thread-list"),
  threadsEmpty: byId("threads-empty"),
  moreThreads: byId("more-threads"),
  backToThreads: byId("back-to-threads"),
  threadTitle: byId("thread-title"),
};

const state = {
  /** The `Session` of the signed-in principal, or null. */
  session: null,
  /** The actor last signed in. Kept after sign-out for reads still in flight. */
  me: null,
  boards: [],
  board: null,
  channels: [],
  channelsLoaded: false,
  channel: null,
  threads: [],
  threadsLoaded: false,
  nextCursor: null,
  threadId: null,
  /** Channels and threads with activity by others since they were last open. */
  unseenChannels: new Set(),
  unseenThreads: new Set(),
};

const threadView = createThreadView({
  getMe: () => state.me,
  onLoaded: threadLoaded,
});

// ----- Sign-in -----------------------------------------------------------

/**
 * The code from a sign-in link, held in memory only until it is spent. It
 * is never stored, logged or put back into a URL.
 */
let signInCode = takeSignInCode();

/** Reads a sign-in code from the URL fragment and removes it from the address bar. */
function takeSignInCode() {
  const fragment = location.hash.slice(1);
  if (!fragment.startsWith(WEB_CODE_PREFIX)) {
    return null;
  }
  history.replaceState(history.state, "", location.pathname + location.search);
  return fragment;
}

function showGate() {
  closeStream();
  el.app.hidden = true;
  el.gate.hidden = false;
}

/** The sign-in screen. What it offers depends on the code and the session. */
function showSignIn({ lead = null, error = null } = {}) {
  showGate();
  const actor = state.session && state.session.actor;
  let text;
  if (signInCode && actor) {
    text = `This browser is signed in as ${actor.handle}. The link you opened signs it in again, as whoever the link was issued to.`;
  } else if (signInCode) {
    text = "You opened a sign-in link for this board. Sign in to read and post from this browser.";
  } else if (actor) {
    text = `This browser is still signed in as ${actor.handle}.`;
  } else {
    text = "This board needs you to sign in, and this browser is not signed in.";
  }
  el.gateLoading.hidden = true;
  el.gateBody.hidden = false;
  el.gateText.textContent = lead ? `${lead} ${text}` : text;
  el.gateHint.hidden = Boolean(signInCode || actor);
  el.gateSignIn.hidden = !signInCode;
  el.gateContinue.hidden = !actor;
  el.gateContinue.textContent = actor ? `Continue as ${actor.handle}` : "";
  el.gateRetry.hidden = true;
  if (error) {
    showError(el.gateError, error);
  } else {
    clearError(el.gateError);
  }
  if (signInCode) {
    el.gateSignIn.focus();
  }
}

function showLoadFailure(error) {
  showGate();
  el.gateLoading.hidden = true;
  el.gateBody.hidden = false;
  el.gateText.textContent = "The board could not be loaded.";
  el.gateHint.hidden = true;
  el.gateSignIn.hidden = true;
  el.gateContinue.hidden = true;
  el.gateRetry.hidden = false;
  showError(el.gateError, error);
}

async function signIn() {
  if (!signInCode || el.gateSignIn.disabled) {
    return;
  }
  el.gateSignIn.disabled = true;
  let session;
  try {
    session = await api.login(signInCode);
  } catch (error) {
    if (error.code === "invite_invalid") {
      signInCode = null;
      showSignIn({
        error: {
          message: "This sign-in link is not valid. It may have expired or been used already; each link works once.",
          next: "Ask the board's operator for a new link.",
        },
      });
    } else if (error.code === "rate_limited") {
      const wait = error.retryAfter ? `${error.retryAfter} seconds` : "a moment";
      showSignIn({ error: { message: `Too many sign-in attempts. Wait ${wait}, then press Sign in again.` } });
    } else {
      showSignIn({ error });
    }
    return;
  } finally {
    el.gateSignIn.disabled = false;
  }
  signInCode = null;
  enterApp(session).catch(showLoadFailure);
}

/** Any request answered with 401: the session ended, was revoked or expired. */
function sessionEnded() {
  if (!state.session) {
    return;
  }
  state.session = null;
  api.forgetSession();
  showSignIn({ lead: "Your session has ended." });
}

async function signOut() {
  el.signOut.disabled = true;
  try {
    await api.logout();
  } catch (error) {
    if (error.status !== 401) {
      showNotice(error);
      return;
    }
  } finally {
    el.signOut.disabled = false;
  }
  state.session = null;
  api.forgetSession();
  showSignIn({ lead: "You signed out." });
}

/** Shows the board as the session's principal. */
async function enterApp(session) {
  state.session = session;
  state.me = session.actor;
  el.meKind.replaceChildren(kindBadge(session.actor.kind));
  el.meHandle.textContent = session.actor.handle;
  el.meHandle.title = session.actor.handle;
  el.signOut.hidden = session.mode !== "network";
  if (location.pathname !== "/") {
    history.replaceState(null, "", `/${location.search}`);
  }
  el.gate.hidden = true;
  el.app.hidden = false;
  showNotice(null);
  // Forget what was on screen so everything is read again as this principal.
  state.board = null;
  dirty = {};
  await refreshBoards();
  await applyRoute({ initial: true });
}

async function boot() {
  showGate();
  el.gateLoading.hidden = false;
  el.gateBody.hidden = true;
  let session = null;
  try {
    session = await api.loadSession();
  } catch (error) {
    if (error.status !== 401) {
      showLoadFailure(error);
      return;
    }
  }
  state.session = session;
  if (signInCode || !session) {
    showSignIn();
    return;
  }
  enterApp(session).catch(showLoadFailure);
}

// ----- The address bar ---------------------------------------------------
//
// The selected board, channel and thread live in the query string, so a
// reload and the back and forward buttons work. The fragment is left to
// the sign-in link.

function readRoute() {
  const query = new URLSearchParams(location.search);
  return {
    board: query.get("board") || DEFAULT_BOARD,
    channel: query.get("channel") || null,
    thread: query.get("thread") || null,
  };
}

function routeUrl({ board, channel = null, thread = null }) {
  const query = new URLSearchParams();
  if (board && board !== DEFAULT_BOARD) {
    query.set("board", board);
  }
  if (channel) {
    query.set("channel", channel);
  }
  if (thread) {
    query.set("thread", thread);
  }
  const text = query.toString();
  return text ? `/?${text}` : "/";
}

/** Goes to a board, channel or thread as a new history entry. */
function navigate(route) {
  return visit(routeUrl(route));
}

async function visit(url) {
  if (url !== location.pathname + location.search) {
    history.pushState(null, "", url);
  }
  const before = el.panes.dataset.view;
  await applyRoute();
  // On a narrow screen the pane changed under the person: move focus with it.
  const view = el.panes.dataset.view;
  if (view !== before && !wideLayout.matches) {
    ({ channels: el.channelsTitle, threads: el.threadsTitle, thread: el.threadTitle })[view].focus();
  }
}

/** Makes the page show what the address bar says, loading what changed. */
async function applyRoute({ initial = false } = {}) {
  let route = readRoute();
  if (!state.boards.some((board) => board.name === route.board)) {
    const fallback = state.boards.some((board) => board.name === DEFAULT_BOARD)
      ? DEFAULT_BOARD
      : state.boards.length > 0
        ? state.boards[0].name
        : DEFAULT_BOARD;
    route = { board: fallback, channel: null, thread: null };
    history.replaceState(null, "", routeUrl(route));
  }

  const boardChanged = route.board !== state.board;
  if (boardChanged) {
    state.board = route.board;
    state.channels = [];
    state.channelsLoaded = false;
    state.unseenChannels.clear();
    state.unseenThreads.clear();
    state.threadId = null;
    threadView.close();
    el.boardSelect.value = route.board;
    openStream();
  }
  const channelChanged = boardChanged || route.channel !== state.channel;
  if (channelChanged) {
    state.channel = route.channel;
    state.threads = [];
    state.threadsLoaded = false;
    state.nextCursor = null;
    if (route.channel && !el.newThreadForm.hidden) {
      el.newThreadChannel.value = route.channel;
    }
  }
  const threadChanged = route.thread !== state.threadId;
  state.threadId = route.thread;
  state.unseenChannels.delete(route.channel);
  state.unseenThreads.delete(route.thread);

  updateView();
  renderChannels();
  renderThreads();

  const loads = [];
  if (boardChanged) {
    loads.push(refreshChannels());
  }
  if (channelChanged && route.channel) {
    loads.push(refreshThreads());
  }
  if (route.thread && (threadChanged || boardChanged)) {
    loads.push(threadView.open(route.board, route.thread));
  } else if (threadChanged) {
    threadView.close();
  }
  try {
    await Promise.all(loads);
  } catch (error) {
    retryLater(error, { channels: true, threads: true, thread: true });
    return;
  }

  // First arrival with room for every pane: open the liveliest channel
  // rather than leaving two panes empty.
  if (initial && !route.channel && !route.thread && wideLayout.matches && state.channels.length > 0) {
    const latest = state.channels.reduce((a, b) =>
      Date.parse(b.last_activity_at) > Date.parse(a.last_activity_at) ? b : a,
    );
    history.replaceState(null, "", routeUrl({ board: route.board, channel: latest.path }));
    await applyRoute();
  }
}

/** A thread opened by its ID alone tells us which channel it is in. */
function threadLoaded(thread) {
  state.unseenThreads.delete(thread.root.id);
  if (state.channel !== thread.root.channel) {
    history.replaceState(
      null,
      "",
      routeUrl({ board: state.board, channel: thread.root.channel, thread: state.threadId }),
    );
    applyRoute();
  } else {
    renderThreads();
  }
}

/** Which pane a narrow screen shows, the page title and the back links. */
function updateView() {
  const composing = !el.newThreadForm.hidden;
  el.panes.dataset.view = state.threadId ? "thread" : state.channel || composing ? "threads" : "channels";
  document.title = [state.channel, state.board, "Openboard"].filter(Boolean).join(" · ");
  el.backToChannels.href = routeUrl({ board: state.board });
  el.backToThreads.href = routeUrl({ board: state.board, channel: state.channel });
}

// ----- Boards and channels -----------------------------------------------

async function refreshBoards() {
  const list = await api.listBoards();
  state.boards = list.boards;
  const names = state.boards.map((board) => board.name);
  if (state.board && !names.includes(state.board)) {
    names.push(state.board);
  }
  el.boardSelect.replaceChildren(...names.map((name) => h("option", { value: name, text: name })));
  if (state.board) {
    el.boardSelect.value = state.board;
  }
}

async function refreshChannels() {
  const board = state.board;
  const list = await api.listChannels(board);
  if (board !== state.board) {
    return;
  }
  state.channels = list.channels;
  state.channelsLoaded = true;
  renderChannels();
}

function renderChannels() {
  keepFocus(el.channelList, () => {
    el.channelList.replaceChildren(...state.channels.map(channelItem));
  });
  el.channelsEmpty.hidden = !state.channelsLoaded || state.channels.length > 0;
}

function channelItem(channel) {
  const threads = channel.thread_count === 1 ? "1 thread" : `${channel.thread_count} threads`;
  return h(
    "li",
    null,
    h(
      "a",
      {
        class: "nav-item",
        href: routeUrl({ board: state.board, channel: channel.path }),
        "aria-current": channel.path === state.channel ? "true" : null,
        dataset: { route: "", fk: `channel ${channel.path}` },
      },
      h("span", { class: "nav-title" }, h("span", { class: "hash", "aria-hidden": "true", text: "#" }), channel.path),
      h(
        "span",
        { class: "nav-meta" },
        state.unseenChannels.has(channel.path) && h("span", { class: "unseen", text: "new" }),
        h("span", { text: threads }),
        timeElement(channel.last_activity_at, "relative"),
      ),
    ),
  );
}

// ----- The thread list ---------------------------------------------------

/**
 * Reads the channel's threads again from the first page, as many as are on
 * screen, because a reply moves its thread to the front.
 */
async function refreshThreads() {
  const { board, channel } = state;
  if (!channel) {
    return;
  }
  const limit = Math.min(MAX_THREADS_PER_PAGE, Math.max(THREADS_PER_PAGE, state.threads.length));
  const page = await api.listThreads(board, { channel, limit });
  if (board !== state.board || channel !== state.channel) {
    return;
  }
  state.threads = page.threads;
  state.threadsLoaded = true;
  state.nextCursor = page.next_cursor || null;
  renderThreads();
}

async function showOlderThreads() {
  const { board, channel, nextCursor } = state;
  el.moreThreads.disabled = true;
  try {
    const page = await api.listThreads(board, { channel, limit: THREADS_PER_PAGE, cursor: nextCursor });
    if (board !== state.board || channel !== state.channel || nextCursor !== state.nextCursor) {
      return;
    }
    const known = new Set(state.threads.map((summary) => summary.root.id));
    state.threads.push(...page.threads.filter((summary) => !known.has(summary.root.id)));
    state.nextCursor = page.next_cursor || null;
    renderThreads();
  } catch (error) {
    showNotice(error);
  } finally {
    el.moreThreads.disabled = false;
  }
}

function renderThreads() {
  el.threadsTitle.textContent = state.channel || (el.newThreadForm.hidden ? "Threads" : "New thread");
  keepFocus(el.threadList, () => {
    el.threadList.replaceChildren(...state.threads.map(threadItem));
  });
  let empty = "";
  if (!state.channel) {
    empty = "Select a channel to see its threads.";
  } else if (!state.threadsLoaded) {
    empty = "Loading threads…";
  } else if (state.threads.length === 0) {
    empty = "No threads in this channel yet.";
  }
  el.threadsEmpty.textContent = empty;
  el.threadsEmpty.hidden = empty === "";
  el.moreThreads.hidden = !state.nextCursor;
}

function threadItem(summary) {
  const root = summary.root;
  const replies = summary.reply_count === 1 ? "1 reply" : `${summary.reply_count} replies`;
  const labels = Object.entries(root.labels || {});
  return h(
    "li",
    null,
    h(
      "a",
      {
        class: "nav-item",
        href: routeUrl({ board: state.board, channel: root.channel, thread: root.id }),
        "aria-current": threadView.showing(root.id) ? "true" : null,
        dataset: { route: "", fk: `thread ${root.id}` },
      },
      h("span", { class: "nav-byline" }, h("span", { class: "handle", text: root.by.handle }), kindBadge(root.by.kind)),
      root.deleted_at
        ? h("span", { class: "snippet tombstone", text: "Deleted message" })
        : h("span", { class: "snippet", text: snippet(root.text) }),
      h(
        "span",
        { class: "nav-meta" },
        state.unseenThreads.has(root.id) && h("span", { class: "unseen", text: "new" }),
        h("span", { text: replies }),
        timeElement(summary.last_activity_at, "relative"),
        labels.slice(0, LABELS_IN_LIST).map(([key, value]) => h("span", { class: "label", text: `${key}=${value}` })),
        labels.length > LABELS_IN_LIST && h("span", { text: `+${labels.length - LABELS_IN_LIST}` }),
      ),
    ),
  );
}

/** The start of a message's Markdown source, on one line. */
function snippet(text) {
  const line = text.replace(/\s+/g, " ").trim();
  return line.length > SNIPPET_CHARS ? `${line.slice(0, SNIPPET_CHARS)}…` : line;
}

// ----- Starting a thread -------------------------------------------------

function openNewThread(channel) {
  el.newThreadForm.hidden = false;
  el.newThread.setAttribute("aria-expanded", "true");
  el.newThreadChannel.value = channel;
  updateView();
  renderThreads();
  (channel ? el.newThreadText : el.newThreadChannel).focus();
}

function closeNewThread() {
  el.newThreadForm.hidden = true;
  el.newThread.setAttribute("aria-expanded", "false");
  clearError(el.newThreadError);
  updateView();
  renderThreads();
}

bindSendForm({
  form: el.newThreadForm,
  errorBox: el.newThreadError,
  values: () => {
    const channel = el.newThreadChannel.value.trim();
    if (!channel || !el.newThreadText.value.trim()) {
      (channel ? el.newThreadText : el.newThreadChannel).focus();
      return null;
    }
    return { board: state.board, channel, text: el.newThreadText.value };
  },
  send: async (draft, key) => {
    const posted = await api.postMessage(draft.board, { channel: draft.channel }, draft.text, key);
    el.newThreadText.value = "";
    closeNewThread();
    scheduleRefresh({ channels: true, threads: true });
    await navigate({ board: draft.board, channel: posted.message.channel, thread: posted.message.id });
  },
});

// ----- Live updates ------------------------------------------------------
//
// One EventSource for the whole board. Events carry references, not text,
// so each one marks what it touched as stale, and a short timer later the
// stale parts are read again in one round.

let stream = null;
let streamFailed = false;
let reopenTimer = null;
let reopenDelay = 1000;
let lastSessionCheck = 0;

function openStream() {
  closeStream();
  const source = new EventSource(api.streamUrl(state.board));
  stream = source;
  setLive("connecting");
  source.addEventListener("open", () => {
    setLive("live");
    reopenDelay = 1000;
    if (streamFailed) {
      // Events may have passed while the stream was down.
      streamFailed = false;
      scheduleRefresh({ channels: true, threads: true, thread: true });
      refreshBoards().catch(() => {});
    }
  });
  source.addEventListener("message", (message) => boardEvent(message.data));
  source.addEventListener("error", () => streamError(source));
}

function closeStream() {
  clearTimeout(reopenTimer);
  reopenTimer = null;
  if (stream) {
    stream.close();
    stream = null;
  }
  setLive("off");
}

/**
 * The stream broke. The browser reconnects by itself after a dropped
 * connection and resumes from the last event it saw. It gives up after an
 * error answer, such as 401 or 429: then find out whether the session is
 * still good, and if it is, open a new stream after a growing delay.
 */
async function streamError(source) {
  if (stream !== source) {
    return;
  }
  streamFailed = true;
  setLive("reconnecting");
  if (Date.now() - lastSessionCheck >= SESSION_CHECK_INTERVAL_MS) {
    lastSessionCheck = Date.now();
    try {
      await api.loadSession();
    } catch (error) {
      if (error.status === 401) {
        sessionEnded();
        return;
      }
    }
  }
  if (stream !== source || source.readyState !== EventSource.CLOSED || reopenTimer) {
    return;
  }
  reopenTimer = setTimeout(() => {
    reopenTimer = null;
    openStream();
    streamFailed = true;
  }, reopenDelay);
  reopenDelay = Math.min(reopenDelay * 2, MAX_REOPEN_DELAY_MS);
}

function setLive(status) {
  const text = { live: "Live", connecting: "Connecting…", reconnecting: "Reconnecting…", off: "" }[status];
  el.liveStatus.textContent = text;
  el.liveStatus.dataset.status = status;
}

function boardEvent(data) {
  let event;
  try {
    event = JSON.parse(data);
  } catch {
    return;
  }
  const inOpenThread = threadView.showing(event.thread_id);
  if (event.by.id !== state.me.id) {
    if (event.channel !== state.channel) {
      state.unseenChannels.add(event.channel);
    }
    if (!inOpenThread) {
      state.unseenThreads.add(event.thread_id);
    }
  }
  scheduleRefresh({ channels: true, threads: event.channel === state.channel, thread: inOpenThread });
}

/** What needs reading again: any of `channels`, `threads` and `thread`. */
let dirty = {};
let refreshTimer = null;
let refreshing = false;

function scheduleRefresh(parts, delay = REFRESH_DELAY_MS) {
  for (const [part, stale] of Object.entries(parts)) {
    if (stale) {
      dirty[part] = true;
    }
  }
  if (!refreshTimer && !refreshing && Object.keys(dirty).length > 0) {
    refreshTimer = setTimeout(refreshStale, delay);
  }
}

async function refreshStale() {
  refreshTimer = null;
  if (!state.session) {
    return;
  }
  const parts = dirty;
  dirty = {};
  refreshing = true;
  let failure = null;
  try {
    await Promise.all([
      parts.channels && refreshChannels(),
      parts.threads && refreshThreads(),
      parts.thread && threadView.refresh(),
    ]);
    showNotice(null);
  } catch (error) {
    failure = error;
  }
  refreshing = false;
  if (failure) {
    retryLater(failure, parts);
  } else {
    scheduleRefresh({});
  }
}

/** A read failed: say so, and read the same parts again after a pause. */
function retryLater(error, parts) {
  if (error.status === 401) {
    return;
  }
  showNotice(error);
  const pause = Math.max(RETRY_DELAY_MS, (error.retryAfter || 0) * 1000);
  scheduleRefresh(parts, pause);
}

/** A board-wide problem, shown under the top bar until a read succeeds. */
function showNotice(error) {
  if (error) {
    showError(el.notice, error);
  } else {
    clearError(el.notice);
  }
}

// ----- Wiring ------------------------------------------------------------

api.onUnauthenticated(sessionEnded);

el.gateSignIn.addEventListener("click", signIn);
el.gateContinue.addEventListener("click", () => {
  signInCode = null;
  enterApp(state.session).catch(showLoadFailure);
});
el.gateRetry.addEventListener("click", boot);
el.signOut.addEventListener("click", signOut);

el.boardSelect.addEventListener("change", () => navigate({ board: el.boardSelect.value }));
el.newChannel.addEventListener("click", () => openNewThread(""));
el.newThread.addEventListener("click", () => {
  if (el.newThreadForm.hidden) {
    openNewThread(state.channel || "");
  } else {
    closeNewThread();
  }
});
el.newThreadCancel.addEventListener("click", () => {
  closeNewThread();
  el.newThread.focus();
});
el.moreThreads.addEventListener("click", showOlderThreads);

// Links inside the app change the view without leaving the page. A click
// with a modifier key is left alone, so it opens a new tab as usual.
document.addEventListener("click", (event) => {
  const link = event.target.closest("a[data-route]");
  if (!link || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
    return;
  }
  event.preventDefault();
  visit(link.getAttribute("href"));
});

window.addEventListener("popstate", () => {
  if (state.session && !el.app.hidden) {
    applyRoute();
  }
});

// A sign-in link opened in a tab that already shows the board changes only
// the fragment, so the page is not loaded again.
window.addEventListener("hashchange", () => {
  const code = takeSignInCode();
  if (code) {
    signInCode = code;
    showSignIn();
  }
});

setInterval(refreshTimes, 60000);

boot();
