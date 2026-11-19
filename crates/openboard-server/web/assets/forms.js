// Form behaviour shared by every place a person writes something.

import { h } from "./dom.js";

/** Shows an API error, its message and its suggested next step, in `box`. */
export function showError(box, error) {
  box.replaceChildren(h("p", { text: error.message }));
  if (error.next) {
    box.append(h("p", { class: "next", text: error.next }));
  }
  box.hidden = false;
}

export function clearError(box) {
  box.replaceChildren();
  box.hidden = true;
}

function newIdempotencyKey() {
  if (crypto.randomUUID) {
    return crypto.randomUUID();
  }
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/**
 * Wires a form that sends something to the board.
 *
 * The form is never submitted natively. Pressing its submit button, or
 * Ctrl+Enter or ⌘+Enter in any of its fields, calls `values()` and then
 * `send(values, key)`. `key` is an idempotency key that stays the same
 * while the values do, so sending again after a failure whose outcome is
 * unknown cannot post twice. A failure is shown in `errorBox`.
 */
export function bindSendForm({ form, errorBox, values, send }) {
  let attempt = null;
  let busy = false;

  async function run() {
    if (busy) {
      return;
    }
    const current = values();
    if (current === null) {
      return;
    }
    const signature = JSON.stringify(current);
    if (!attempt || attempt.signature !== signature) {
      attempt = { signature, key: newIdempotencyKey() };
    }
    busy = true;
    form.setAttribute("aria-busy", "true");
    const submit = form.querySelector("[type=submit]");
    submit.disabled = true;
    try {
      await send(current, attempt.key);
      attempt = null;
      clearError(errorBox);
    } catch (error) {
      showError(errorBox, error);
    } finally {
      busy = false;
      form.removeAttribute("aria-busy");
      submit.disabled = false;
    }
  }

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    run();
  });
  form.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      form.requestSubmit();
    }
  });
}
