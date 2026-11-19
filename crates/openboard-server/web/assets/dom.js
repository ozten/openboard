// Small DOM helpers shared by the views.
//
// Nothing here parses HTML. Text reaches the page through `textContent`
// and attributes through `setAttribute`, so untrusted strings stay text.

/**
 * Builds an element. `props` may hold `class`, `text`, `on` (event name to
 * listener) and `dataset`; every other key becomes an attribute, skipped
 * when its value is null, undefined or false. Children are nodes or strings.
 */
export function h(tag, props, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) {
      continue;
    }
    if (name === "class") {
      element.className = value;
    } else if (name === "text") {
      element.textContent = value;
    } else if (name === "on") {
      for (const [type, listener] of Object.entries(value)) {
        element.addEventListener(type, listener);
      }
    } else if (name === "dataset") {
      Object.assign(element.dataset, value);
    } else {
      element.setAttribute(name, value === true ? "" : String(value));
    }
  }
  element.append(...children.flat().filter((child) => child !== null && child !== undefined && child !== false));
  return element;
}

/**
 * Runs `rebuild`, which replaces nodes inside `container`, and afterwards
 * puts keyboard focus back on the element carrying the same `data-fk` key
 * as the one that had it.
 */
export function keepFocus(container, rebuild) {
  const active = document.activeElement;
  const key = active && container.contains(active) ? active.dataset.fk : undefined;
  rebuild();
  if (key !== undefined) {
    focusKey(container, key);
  }
}

/**
 * Focuses the element inside `container` whose `data-fk` equals `key`.
 * Returns whether there was one.
 */
export function focusKey(container, key) {
  for (const candidate of container.querySelectorAll("[data-fk]")) {
    if (candidate.dataset.fk === key) {
      candidate.focus({ preventScroll: true });
      return true;
    }
  }
  return false;
}

// ----- Who wrote it ------------------------------------------------------

const KINDS = {
  person: "person",
  agent: "agent",
  local: "local",
  service: "service",
  bridge: "bridge",
};

/** The actor kind as one of the known names, or "unknown". */
export function kindName(kind) {
  return Object.hasOwn(KINDS, kind) ? KINDS[kind] : "unknown";
}

/** A text badge that says what kind of principal an actor is. */
export function kindBadge(kind) {
  const name = kindName(kind);
  return h("span", { class: `kind kind-${name}`, text: name });
}

// ----- Time --------------------------------------------------------------

const fullTime = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const clockTime = new Intl.DateTimeFormat(undefined, { timeStyle: "short" });
const dayOnly = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });

function timeText(date, style) {
  const now = new Date();
  if (style === "clock") {
    const sameDay = date.toDateString() === now.toDateString();
    return sameDay ? clockTime.format(date) : fullTime.format(date);
  }
  const seconds = Math.max(0, (now - date) / 1000);
  if (seconds < 45) {
    return "just now";
  }
  if (seconds < 3600) {
    return `${Math.max(1, Math.round(seconds / 60))} min ago`;
  }
  if (seconds < 86400) {
    return `${Math.round(seconds / 3600)} h ago`;
  }
  if (seconds < 7 * 86400) {
    return `${Math.round(seconds / 86400)} d ago`;
  }
  return dayOnly.format(date);
}

/**
 * A `<time>` for an RFC 3339 timestamp. `style` is "relative" ("5 min
 * ago") or "clock" (the time today, the date and time otherwise).
 */
export function timeElement(iso, style) {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) {
    return h("time", { text: "" });
  }
  return h("time", {
    datetime: date.toISOString(),
    title: fullTime.format(date),
    dataset: { style },
    text: timeText(date, style),
  });
}

/** Brings every time on the page up to date, since "5 min ago" goes stale. */
export function refreshTimes() {
  for (const element of document.querySelectorAll("time[data-style]")) {
    element.textContent = timeText(new Date(element.dateTime), element.dataset.style);
  }
}
