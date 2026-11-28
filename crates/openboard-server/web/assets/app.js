// The Openboard web UI: sign-in, the board, channel and thread lists, the
// address bar, and the one live event stream. The thread pane is thread.js.

import * as api from "./api.js";
import { TASK_LABEL, h, keepFocus, kindBadge, refreshTimes, taskBadge, timeElement } from "./dom.js";
import { bindSendForm, clearError, showError } from "./forms.js";
import { createPresenceView } from "./presence.js";
import { createThreadView } from "./thread.js";

const DEFAULT_BOARD = "general";
const WEB_CODE_PREFIX = "obw1_";
const THREADS_PER_PAGE = 50;
const MAX_THREADS_PER_PAGE = 500;
const SNIPPET_CHARS = 200;
const LABELS_IN_LIST = 3;
/** Events arriving within this time are answered by one round of reads. */
const REFRESH_DELAY_MS = 300;
/** A read that failed for a passing reason is tried again after this long, doubling up to the cap. */
const RETRY_DELAY_MS = 5000;
const MAX_RETRY_DELAY_MS = 60000;
const SESSION_CHECK_INTERVAL_MS = 10000;
const MAX_REOPEN_DELAY_MS = 30000;
/** A tab hidden for longer than this gives up its event stream. */
const HIDDEN_GRACE_MS = 3000;
/** At this width all three panes show at once; below it, one at a time. */
const wideLayout = window.matchMedia("(min-width: 900px)");
const EVERYTHING = { channels: true, threads: true, thread: true };

const byId = (id) => document.getElementById(id);
const el = {
  gate: byId("gate"),
  gateTitle: byId("gate-title"),
  gateLoading: byId("gate-loading"),
  gateBody: byId("gate-body"),
  gateText: byId("gate-text"),
  gateHint: byId("gate-hint"),
  gateError: byId("gate-error"),
  gateSignIn: byId("gate-sign-in"),
  gateContinue: byId("gate-continue"),
  gateRetry: byId("gate-retry"),
  app: byId("app"),
  brand: byId("brand"),
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

const presence = createPresenceView({
  getBoard: () => state.board,
  channelUrl: (channel) => routeUrl({ board: state.board, channel }),
});

const threadView = createThreadView({
  getMe: () => state.me,
  onLoaded: threadLoaded,
  dotFor: presence.dotFor,
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

/**
 * The sign-in screen. What it offers depends on the code and the session.
 * `fromApp` says the person was just looking at the board, so keyboard
 * focus has to be given somewhere on this screen.
 */
function showSignIn({ lead = null, error = null, fromApp = false } = {}) {
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
  } else if (fromApp) {
    el.gateTitle.focus();
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

/**
 * The session is over: signed out here, or ended, expired or revoked
 * elsewhere. Nothing of the board may stay on the page behind the sign-in
 * screen, so the lists, the open thread and every draft are emptied.
 */
function endSession(lead) {
  state.session = null;
  api.forgetSession();
  clearTimeout(hiddenTimer);
  clearTimeout(refreshTimer);
  hiddenTimer = null;
  refreshTimer = null;
  stale = {};
  threadView.reset();
  state.boards = [];
  state.board = null;
  state.channels = [];
  state.channelsLoaded = false;
  state.channel = null;
  state.threads = [];
  state.threadsLoaded = false;
  state.nextCursor = null;
  state.threadId = null;
  state.unseenChannels.clear();
  state.unseenThreads.clear();
  el.boardSelect.replaceChildren();
  el.newThreadChannel.value = "";
  el.newThreadText.value = "";
  closeNewThread();
  renderChannels();
  el.meKind.replaceChildren();
  el.meHandle.textContent = "";
  el.meHandle.removeAttribute("title");
  showNotice(null);
  document.title = "Openboard";
  showSignIn({ lead, fromApp: true });
}

/** Any request answered with 401. */
function sessionEnded() {
  if (state.session) {
    endSession("Your session has ended.");
  }
}

/**
 * Another tab signed this browser in as someone else. Show the board as
 * that person, say so, and keep what was being written in its box.
 */
function identityChanged(session) {
  const was = state.me ? state.me.handle : "someone else";
  const notice = {
    message: `This browser is now signed in as ${session.actor.handle}, not ${was}: the sign-in changed in another tab.`,
    next: "Nothing you were sending went out. What you had written is still in its box; send it again if you want to post it as this person.",
  };
  enterApp(session, notice).catch(showLoadFailure);
}

async function signOut() {
  el.signOut.disabled = true;
  try {
    await api.logout();
  } catch (error) {
    if (error.code === "identity_changed") {
      return;
    }
    if (error.status !== 401) {
      showNotice(error);
      return;
    }
  } finally {
    el.signOut.disabled = false;
  }
  endSession("You signed out.");
}

/** Shows the board as the session's principal, under `notice` if there is one. */
async function enterApp(session, notice = null) {
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
  showNotice(notice);
  // Forget what was on screen so everything is read again as this principal.
  state.board = null;
  stale = {};
  await refreshBoards();
  await applyRoute({ initial: true });
}

/** Puts the server's version and commit in the brand's tooltip. Never blocks the page. */
function showServerVersion() {
  api.loadHealth().then(
    (health) => {
      const parts = [`Openboard ${health.version}`];
      if (health.commit) {
        parts.push(`(${health.commit})`);
      }
      if (health.api_level) {
        parts.push(`· API level ${health.api_level}`);
      }
      el.brand.title = parts.join(" ");
    },
    () => {},
  );
}

async function boot() {
  showServerVersion();
  showGate();
  el.gateLoading.hidden = false;
  el.gateBody.hidden = true;
  let session = null;
  let failure = null;
  try {
    session = await api.loadSession();
  } catch (error) {
    if (error.status !== 401) {
      failure = error;
    }
  }
  state.session = session;
  if (signInCode) {
    // Whatever the probe said, signing in with the code may still work.
    showSignIn();
  } else if (failure) {
    showLoadFailure(failure);
  } else if (!session) {
    showSignIn();
  } else {
    enterApp(session).catch(showLoadFailure);
  }
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
    presence.reset();
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
    loads.push(["channels", refreshChannels()]);
  }
  if (channelChanged && route.channel) {
    loads.push(["threads", refreshThreads()]);
  }
  if (route.thread && (threadChanged || boardChanged)) {
    loads.push(["thread", threadView.open(route.board, route.thread)]);
  } else if (threadChanged) {
    threadView.close();
  }
  const results = await Promise.allSettled(loads.map(([, load]) => load));
  let rejected = false;
  for (const [index, result] of results.entries()) {
    if (result.status === "rejected") {
      rejected = true;
      if (selectionFailed(route, loads[index][0], result.reason)) {
        // The address bar now names something that exists: show that.
        await applyRoute();
        return;
      }
    }
  }
  if (rejected) {
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

/**
 * One of the reads for a selection failed. A passing failure is tried
 * again later. Any other is the board's answer about the selection itself,
 * for example a channel path that is not valid: asking again would only
 * repeat it, so it is shown once and the selection is taken out of the
 * address bar. Returns true when the address bar was changed.
 */
function selectionFailed(route, part, error) {
  if (error.status === 401 || error.code === "identity_changed") {
    return false;
  }
  if (error.transient) {
    readFailed(error, { [part]: true });
    return false;
  }
  showNotice(error);
  let corrected = null;
  if (part === "thread" && state.threadId === route.thread) {
    corrected = { board: route.board, channel: route.channel };
  } else if (part === "threads" && state.channel === route.channel) {
    corrected = { board: route.board, thread: route.thread };
  } else if (part === "channels" && state.board === route.board && route.board !== DEFAULT_BOARD) {
    corrected = { board: DEFAULT_BOARD };
  }
  if (!corrected) {
    return false;
  }
  history.replaceState(null, "", routeUrl(corrected));
  return true;
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
  if (!state.session) {
    return;
  }
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
    if (error.status !== 401) {
      showNotice(error);
    }
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
  const labels = Object.entries(root.labels || {}).filter(([key]) => key !== TASK_LABEL);
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
        root.task && taskBadge(root.task.state),
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
  const line = text.slice(0, SNIPPET_CHARS * 4).replace(/\s+/g, " ").trim();
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
// One EventSource for the whole board, held only while the tab is visible.
// The board speaks HTTP/1.1 and a browser allows about six connections to
// one host, shared by every tab: if each background tab kept a stream, the
// sixth would leave no connection for anyone's requests.
//
// Events carry references, not text, so each one marks what it touched as
// stale, and a short timer later the stale parts are read again in one round.

let stream = null;
/** Set while events may have been missed; the next `open` reads everything again. */
let catchUpOnOpen = false;
/** True while the stream is closed because the tab is in the background. */
let streamPaused = false;
let hiddenTimer = null;
let reopenTimer = null;
let reopenDelay = 1000;
let lastSessionCheck = 0;

function openStream() {
  closeStream();
  if (document.hidden) {
    streamPaused = true;
    setLive("paused");
    return;
  }
  const source = new EventSource(api.streamUrl(state.board));
  stream = source;
  setLive("connecting");
  source.addEventListener("open", () => {
    setLive("live");
    presence.start();
    reopenDelay = 1000;
    if (catchUpOnOpen) {
      catchUpOnOpen = false;
      scheduleRefresh(EVERYTHING);
      refreshBoards().catch(() => {});
    }
  });
  source.addEventListener("message", (message) => boardEvent(message.data));
  source.addEventListener("error", () => streamError(source));
}

function closeStream() {
  presence.stop();
  clearTimeout(reopenTimer);
  reopenTimer = null;
  streamPaused = false;
  if (stream) {
    stream.close();
    stream = null;
  }
  setLive("off");
}

/** The tab has been in the background long enough: free its connection. */
function pauseStream() {
  hiddenTimer = null;
  closeStream();
  streamPaused = true;
  setLive("paused");
}

/**
 * The tab came to the front. While it was away another tab may have signed
 * this browser out or in as someone else, so ask first. Then, if the
 * stream was given up, read everything again and open a new one, which
 * reads once more when it connects so nothing in between is missed.
 */
async function tabShown() {
  try {
    if (!(await api.recheckSession())) {
      return;
    }
  } catch {
    // The board did not answer. The reads below report that.
  }
  if (!state.session || document.hidden || !streamPaused) {
    return;
  }
  scheduleRefresh(EVERYTHING, 0);
  openStream();
  catchUpOnOpen = true;
}

/**
 * The stream broke. The browser reconnects by itself after a dropped
 * connection and resumes from the last event it saw. It gives up after an
 * error answer. Then find out whether the session and the board are still
 * good with ordinary requests, whose answers can be read, and only if they
 * are, open a new stream after a growing delay.
 */
async function streamError(source) {
  if (stream !== source) {
    return;
  }
  catchUpOnOpen = true;
  setLive("reconnecting");
  if (Date.now() - lastSessionCheck >= SESSION_CHECK_INTERVAL_MS) {
    lastSessionCheck = Date.now();
    try {
      if (!(await api.recheckSession())) {
        return;
      }
    } catch {
      // Unreachable for now. The browser, or the timer below, tries again.
    }
  }
  if (stream !== source || source.readyState !== EventSource.CLOSED || reopenTimer) {
    return;
  }
  try {
    await refreshChannels();
  } catch (error) {
    if (!error.transient) {
      // The board will not serve this board to us; a new stream would be
      // refused the same way.
      if (stream === source) {
        closeStream();
        if (error.status !== 401) {
          showNotice(error);
        }
      }
      return;
    }
  }
  if (stream !== source || reopenTimer) {
    return;
  }
  reopenTimer = setTimeout(() => {
    reopenTimer = null;
    openStream();
    catchUpOnOpen = true;
  }, reopenDelay);
  reopenDelay = Math.min(reopenDelay * 2, MAX_REOPEN_DELAY_MS);
}

function setLive(status) {
  const text = {
    live: "Live",
    connecting: "Connecting…",
    reconnecting: "Reconnecting…",
    paused: "Paused in the background",
    off: "",
  }[status];
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
let stale = {};
let refreshTimer = null;
let refreshing = false;
let retryDelay = RETRY_DELAY_MS;

function scheduleRefresh(parts, delay = REFRESH_DELAY_MS) {
  for (const [part, isStale] of Object.entries(parts)) {
    if (isStale) {
      stale[part] = true;
    }
  }
  if (!refreshTimer && !refreshing && Object.keys(stale).length > 0) {
    refreshTimer = setTimeout(refreshStale, delay);
  }
}

async function refreshStale() {
  refreshTimer = null;
  if (!state.session) {
    return;
  }
  const parts = stale;
  stale = {};
  refreshing = true;
  const results = await Promise.allSettled([
    parts.channels && refreshChannels(),
    parts.threads && refreshThreads(),
    parts.thread && threadView.refresh(),
  ]);
  refreshing = false;
  const failed = results.find((result) => result.status === "rejected");
  if (failed) {
    readFailed(failed.reason, parts);
  } else {
    retryDelay = RETRY_DELAY_MS;
    if (noticeClearsItself) {
      showNotice(null);
    }
  }
  // Whatever events marked stale while this round was running.
  scheduleRefresh({});
}

/**
 * A background read failed. Say so. Read the same parts again only when
 * the failure can pass (the board unreachable, busy or failing), after a
 * pause that doubles each time and honours `Retry-After`. Any other answer
 * would be the same next time, so it is shown and not asked for again.
 */
function readFailed(error, parts) {
  if (!state.session || error.status === 401 || error.code === "identity_changed") {
    return;
  }
  showNotice(error, error.transient);
  if (!error.transient) {
    return;
  }
  const pause = Math.max(retryDelay, (error.retryAfter || 0) * 1000);
  retryDelay = Math.min(retryDelay * 2, MAX_RETRY_DELAY_MS);
  scheduleRefresh(parts, pause);
}

/** True when the notice on screen reports a passing failure, which the next good read clears. */
let noticeClearsItself = false;

/** A board-wide message under the top bar. `null` removes it. */
function showNotice(error, clearsItself = false) {
  noticeClearsItself = Boolean(error) && clearsItself;
  if (!error) {
    clearError(el.notice);
    return;
  }
  showError(el.notice, error);
  el.notice.append(h("button", { type: "button", text: "Dismiss", on: { click: () => showNotice(null) } }));
}

// ----- Wiring ------------------------------------------------------------

api.onUnauthenticated(sessionEnded);
api.onIdentityChanged(identityChanged);

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
  // On a narrow screen with no channel chosen, closing the form leaves
  // the threads pane, and the button that opened it, behind.
  (el.newThread.offsetParent ? el.newThread : el.channelsTitle).focus();
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

document.addEventListener("visibilitychange", () => {
  if (!state.session || el.app.hidden) {
    return;
  }
  clearTimeout(hiddenTimer);
  hiddenTimer = null;
  if (document.hidden) {
    hiddenTimer = setTimeout(pauseStream, HIDDEN_GRACE_MS);
  } else {
    tabShown();
  }
});

// A sign-in link opened where the address already reads `/join`, which is
// the sign-in screen after a link that did not work, differs only in its
// fragment, so the browser does not load the page again. The board itself
// lives at `/`, where a link is always a fresh load; a code that turns up
// there some other way is taken out of the address bar and not used.
window.addEventListener("hashchange", () => {
  const code = takeSignInCode();
  if (code && !el.gate.hidden) {
    signInCode = code;
    showSignIn();
  }
});

setInterval(refreshTimes, 60000);

boot();
