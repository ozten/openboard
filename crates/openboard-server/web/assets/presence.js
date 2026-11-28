// The Here section: who is around on the board, from
// `GET /v1/boards/{board}/presence`, and the presence dots beside authors
// in a thread. A dot is live (green, filled), recent (yellow, half) or away
// (grey, hollow): the shapes differ as well as the colours, and every dot
// carries its state as text for screen readers.
//
// The roster is read when the event stream opens and every 30 seconds while
// it stays open. A tab in the background has no stream, and so no timer.

import * as api from "./api.js";
import { h, keepFocus, kindBadge } from "./dom.js";

const REFRESH_MS = 30000;
/** Remembered per browser: whether the Here list is folded away. */
const COLLAPSED_KEY = "openboard.here.collapsed";
const STATES = ["live", "recent", "away"];

function stateName(state) {
  return STATES.includes(state) ? state : "away";
}

/** How long before the server's `now` the time `then` was, briefly. */
function ago(now, then) {
  const seconds = Math.max(0, (Date.parse(now) - Date.parse(then)) / 1000);
  if (!Number.isFinite(seconds)) {
    return "";
  }
  if (seconds < 60) {
    return "just now";
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m ago`;
  }
  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}h ago`;
  }
  return `${Math.floor(seconds / 86400)}d ago`;
}

function readCollapsed() {
  try {
    return localStorage.getItem(COLLAPSED_KEY) === "1";
  } catch {
    return false;
  }
}

function writeCollapsed(collapsed) {
  try {
    localStorage.setItem(COLLAPSED_KEY, collapsed ? "1" : "0");
  } catch {
    // Storage is off in this browser; the choice lasts until the page closes.
  }
}

/** A presence dot. `actorId` lets later rosters repaint it in place. */
export function presenceDot(state, actorId = null) {
  const name = state ? stateName(state) : null;
  return h("span", {
    class: "presence-dot",
    role: "img",
    "aria-label": name || "",
    "aria-hidden": name ? null : "true",
    title: name || "",
    dataset: actorId ? { state: name || "unknown", actor: actorId } : { state: name || "unknown" },
  });
}

function paint(dot, state) {
  const name = state ? stateName(state) : null;
  dot.dataset.state = name || "unknown";
  dot.title = name || "";
  dot.setAttribute("aria-label", name || "");
  if (name) {
    dot.removeAttribute("aria-hidden");
  } else {
    dot.setAttribute("aria-hidden", "true");
  }
}

/**
 * `getBoard` names the board on screen. `channelUrl(channel)` is the
 * address of a channel on it.
 */
export function createPresenceView({ getBoard, channelUrl }) {
  const el = {
    section: document.getElementById("here"),
    toggle: document.getElementById("here-toggle"),
    count: document.getElementById("here-count"),
    list: document.getElementById("here-list"),
  };
  let roster = null;
  /** Seat state by actor ID, for the dots in threads. */
  const seatStates = new Map();
  /** Principals whose seats are unfolded. */
  const unfolded = new Set();
  let timer = null;
  let running = false;
  /** The board answered that it has no presence: an older server. */
  let unsupported = false;
  let collapsed = readCollapsed();

  applyCollapsed();
  el.toggle.addEventListener("click", () => {
    collapsed = !collapsed;
    writeCollapsed(collapsed);
    applyCollapsed();
  });

  function applyCollapsed() {
    el.toggle.setAttribute("aria-expanded", String(!collapsed));
    el.list.hidden = collapsed;
  }

  /** The stream is open: read now and keep reading. */
  function start() {
    running = true;
    refresh();
  }

  /** The stream closed: stop reading. What is shown stays. */
  function stop() {
    running = false;
    clearTimeout(timer);
    timer = null;
  }

  /** Another board: forget this one's roster. */
  function reset() {
    roster = null;
    unsupported = false;
    unfolded.clear();
    seatStates.clear();
    el.list.replaceChildren();
    el.section.hidden = true;
    paintDots();
  }

  async function refresh() {
    clearTimeout(timer);
    timer = null;
    if (unsupported) {
      return;
    }
    const board = getBoard();
    try {
      const list = await api.getPresence(board);
      if (board !== getBoard()) {
        return;
      }
      roster = list;
      render();
    } catch (error) {
      if (board === getBoard() && error.status === 404) {
        // The board is on screen, so it exists: the server predates presence.
        unsupported = true;
        el.section.hidden = true;
        return;
      }
      // A passing failure: keep what is shown and try at the next tick.
    } finally {
      if (running && !unsupported && board === getBoard() && timer === null) {
        timer = setTimeout(refresh, REFRESH_MS);
      }
    }
  }

  function render() {
    seatStates.clear();
    for (const principal of roster.principals) {
      for (const seat of principal.seats) {
        seatStates.set(seat.actor_id, seat.state);
      }
    }
    const around = roster.principals.filter((principal) => principal.state !== "away").length;
    el.count.textContent = String(around);
    el.toggle.setAttribute(
      "aria-label",
      `Here: ${around} ${around === 1 ? "is" : "are"} around, ${roster.principals.length} seen in the last ${roster.windows.listed_days} days`,
    );
    keepFocus(el.list, () => {
      el.list.replaceChildren(...roster.principals.map(principalItem));
    });
    el.section.hidden = false;
    paintDots();
  }

  function principalItem(principal) {
    const open = unfolded.has(principal.id);
    const state = stateName(principal.state);
    const when = state === "live" ? "" : ago(roster.now, principal.last_seen_at);
    const summary = state === "live" ? seatSummary(principal) : "";
    const disabled =
      principal.disabled === "revoked"
        ? "access revoked"
        : principal.disabled === "expired"
          ? "token expired"
          : null;
    return h(
      "li",
      { class: `here-item here-${state}${disabled ? " here-disabled" : ""}` },
      h(
        "button",
        {
          type: "button",
          class: "here-row",
          "aria-expanded": String(open),
          title: `last seen ${new Date(principal.last_seen_at).toLocaleString()}`,
          dataset: { fk: `here ${principal.id}` },
          on: {
            click: () => {
              if (open) {
                unfolded.delete(principal.id);
              } else {
                unfolded.add(principal.id);
              }
              render();
            },
          },
        },
        h(
          "span",
          { class: "here-line" },
          presenceDot(state),
          h("span", { class: "handle", text: principal.handle }),
          kindBadge(principal.kind),
          (disabled || when) && h("span", { class: "here-meta", text: disabled || when }),
        ),
        summary && h("span", { class: "here-summary", text: summary }),
      ),
      open && h("ul", { class: "here-seats" }, ...principal.seats.map(seatItem)),
    );
  }

  /** The live seats of a principal by profile, `web` for a browser. */
  function seatSummary(principal) {
    const names = principal.seats
      .filter((seat) => seat.state === "live")
      .map((seat) => {
        if (seat.profile) {
          return seat.profile;
        }
        const browser = seat.listening.length > 0 && seat.listening.every((listen) => listen.kind === "stream" && !listen.channel);
        return browser ? "web" : "";
      })
      .filter(Boolean);
    return names.join(", ");
  }

  function seatItem(seat) {
    const state = stateName(seat.state);
    const listening = seat.listening.map((listen, index) => [
      index > 0 && ", ",
      listen.channel
        ? h("a", {
            href: channelUrl(listen.channel.replace(/\/$/, "")),
            dataset: { route: "" },
            text: listen.channel,
          })
        : h("span", { text: listen.kind === "stream" ? "web" : "whole board" }),
    ]);
    return h(
      "li",
      { class: "here-seat" },
      presenceDot(state),
      h("span", { class: "handle", text: seat.handle }),
      listening.length > 0
        ? h("span", { class: "here-meta" }, "listening on ", ...listening.flat())
        : h("span", { class: "here-meta", text: state === "live" ? "live" : ago(roster.now, seat.last_seen_at) }),
    );
  }

  /** Brings every author dot on the page up to date. */
  function paintDots() {
    for (const dot of document.querySelectorAll(".presence-dot[data-actor]")) {
      paint(dot, seatStates.get(dot.dataset.actor));
    }
  }

  /** A dot for an author, painted from the latest roster. */
  function dotFor(actorId) {
    return presenceDot(seatStates.get(actorId), actorId);
  }

  return { start, stop, reset, refresh, dotFor };
}
