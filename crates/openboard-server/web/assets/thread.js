// The thread pane: a root message and its replies, the actions on each
// message, and the reply box.

import * as api from "./api.js";
import { focusKey, h, keepFocus, kindBadge, kindName, timeElement } from "./dom.js";
import { bindSendForm, clearError, showError } from "./forms.js";

/** How a reaction name is drawn. A name that is not here is drawn as `:name:`. */
const REACTION_CHARACTERS = {
  thumbsup: "\u{1F44D}",
  "+1": "\u{1F44D}",
  thumbsdown: "\u{1F44E}",
  "-1": "\u{1F44E}",
  heart: "❤️",
  eyes: "\u{1F440}",
  check: "✅",
  question: "❓",
  tada: "\u{1F389}",
  rocket: "\u{1F680}",
};
/** The reactions offered for adding. */
const OFFERED_REACTIONS = ["thumbsup", "heart", "eyes", "check", "question"];
/** Closer to the end than this counts as reading the newest messages. */
const NEAR_BOTTOM_PX = 80;
const MAX_MENTION_CHIPS = 6;

const CONFLICT_ON_EDIT = {
  message:
    "This message changed while you were editing it. Your text is kept here. " +
    "Cancel to see the newer version, or save again to replace it.",
};
const CONFLICT_ON_DELETE = {
  message: "This message changed after it was loaded, so it was not deleted. Read it again, then delete it.",
};
const BAD_LABEL = { message: "Write a label as key=value, for example status=done." };

function reactionText(name) {
  return Object.hasOwn(REACTION_CHARACTERS, name) ? REACTION_CHARACTERS[name] : `:${name}:`;
}

/**
 * Puts a message's rendered Markdown into the page.
 *
 * This is the only place in the UI that parses a string as HTML, and the
 * only string it may be given is `Message.html`, which the server built
 * through its sanitising allowlist. Nothing else goes through here.
 */
function insertSanitisedMessageHtml(container, messageHtml) {
  container.innerHTML = messageHtml;
}

/**
 * Creates the thread view over the elements of the thread pane.
 *
 * `getMe()` returns the signed-in actor. `onLoaded(thread)` is called when
 * a thread is first shown.
 */
export function createThreadView({ getMe, onLoaded }) {
  const el = {
    title: document.getElementById("thread-title"),
    scroll: document.getElementById("thread-scroll"),
    empty: document.getElementById("thread-empty"),
    messages: document.getElementById("thread-messages"),
    announce: document.getElementById("thread-announce"),
    jump: document.getElementById("jump-latest"),
    replyForm: document.getElementById("reply-form"),
    replyText: document.getElementById("reply-text"),
    replyError: document.getElementById("reply-error"),
    mentionRow: document.getElementById("mention-row"),
  };

  /** The thread on screen: `{ board, id, thread, cards, participants }`. */
  let current = null;
  /** Unsent replies by thread, so moving between threads loses nothing. */
  const drafts = new Map();

  const draftKey = (view) => `${view.board}\n${view.id}`;

  // ----- Opening, closing and refreshing ---------------------------------

  function open(board, id) {
    if (current && current.board === board && current.id === id) {
      return refresh();
    }
    close();
    current = { board, id, thread: null, cards: new Map(), participants: null };
    el.empty.textContent = "Loading the thread…";
    return refresh();
  }

  function close() {
    if (current) {
      if (el.replyText.value) {
        drafts.set(draftKey(current), el.replyText.value);
      } else {
        drafts.delete(draftKey(current));
      }
    }
    current = null;
    el.messages.replaceChildren();
    el.replyText.value = "";
    clearError(el.replyError);
    el.replyForm.hidden = true;
    el.jump.hidden = true;
    el.title.textContent = "Thread";
    el.empty.textContent = "Select a thread to read it.";
    el.empty.hidden = false;
  }

  /** True when the open thread is the one with this root or message ID. */
  function showing(threadId) {
    if (!current) {
      return false;
    }
    return current.id === threadId || (current.thread !== null && current.thread.root.id === threadId);
  }

  /** Fetches the open thread again and redraws what changed. */
  async function refresh() {
    const view = current;
    if (!view) {
      return;
    }
    let thread;
    try {
      thread = await api.getThread(view.board, view.id);
    } catch (error) {
      if (current !== view) {
        return;
      }
      if (!view.thread) {
        el.empty.textContent =
          error.code === "not_found" ? "This thread does not exist on this board." : "The thread could not be loaded.";
        if (error.code === "not_found") {
          return;
        }
      }
      throw error;
    }
    if (current !== view) {
      return;
    }
    const first = view.thread === null;
    view.thread = thread;
    draw(view, first);
    if (first) {
      onLoaded(thread);
    }
  }

  function draw(view, first) {
    const me = getMe();
    const stick = first || nearBottom();
    const arrived = [];
    keepFocus(el.messages, () => {
      for (const message of [view.thread.root, ...view.thread.replies]) {
        const signature = JSON.stringify(message);
        const card = view.cards.get(message.id);
        if (!card) {
          const added = { message, signature, mode: null, error: null, node: null };
          added.node = buildCard(view, added);
          view.cards.set(message.id, added);
          el.messages.append(added.node);
          arrived.push(message);
        } else if (card.signature !== signature) {
          card.message = message;
          card.signature = signature;
          // A card with an open form keeps what the person is typing; it
          // is redrawn from the newest message when the form closes.
          if (!card.mode) {
            redraw(view, card);
          }
        }
      }
    });
    el.title.textContent = view.thread.root.channel;
    el.empty.hidden = true;
    el.replyForm.hidden = false;
    drawMentionRow(view, me);
    if (first) {
      el.replyText.value = drafts.get(draftKey(view)) || "";
    }
    if (stick) {
      scrollToEnd();
    } else if (arrived.length > 0) {
      el.jump.hidden = false;
    }
    const fromOthers = arrived.filter((message) => message.by.id !== me.id);
    if (!first && fromOthers.length > 0) {
      el.announce.textContent = `New reply from ${fromOthers[fromOthers.length - 1].by.handle}`;
    }
  }

  function redraw(view, card) {
    const node = buildCard(view, card);
    card.node.replaceWith(node);
    card.node = node;
  }

  function nearBottom() {
    return el.scroll.scrollHeight - el.scroll.scrollTop - el.scroll.clientHeight < NEAR_BOTTOM_PX;
  }

  function scrollToEnd() {
    el.scroll.scrollTop = el.scroll.scrollHeight;
    el.jump.hidden = true;
  }

  // ----- One message -----------------------------------------------------

  function buildCard(view, card) {
    const message = card.message;
    const me = getMe();
    const mine = message.by.id === me.id;
    const deleted = Boolean(message.deleted_at);
    const kind = kindName(message.by.kind);
    const classes = ["message", `from-${kind}`];
    if (message.id === message.thread_id) {
      classes.push("root");
    }
    if (deleted) {
      classes.push("deleted");
    }

    const node = h(
      "article",
      {
        class: classes.join(" "),
        "aria-label": `Message from ${message.by.handle}, ${kind}`,
        on: {
          keydown: (event) => {
            if (event.key === "Escape" && card.mode) {
              event.stopPropagation();
              setMode(view, card, null, `${message.id} ${card.mode}`);
            }
          },
        },
      },
      h("span", { class: "avatar", "aria-hidden": "true", text: (Array.from(message.by.handle)[0] || "?").toUpperCase() }),
      h(
        "header",
        { class: "message-head" },
        h("span", { class: "handle", text: message.by.handle }),
        kindBadge(message.by.kind),
        mine && h("span", { class: "you", text: "you" }),
        timeElement(message.created_at, "clock"),
        message.edited_at &&
          !deleted &&
          h("span", { class: "edited" }, "edited ", timeElement(message.edited_at, "clock")),
      ),
    );

    if (deleted) {
      node.append(
        h("p", { class: "tombstone" }, "This message was deleted ", timeElement(message.deleted_at, "clock"), "."),
      );
      return node;
    }
    if (card.mode === "edit") {
      node.append(editForm(view, card));
      return node;
    }

    const body = h("div", { class: "body" });
    if (typeof message.html === "string") {
      insertSanitisedMessageHtml(body, message.html);
      finishBody(body, me);
    } else {
      body.classList.add("plain");
      body.textContent = message.text;
    }
    node.append(body, chips(view, card, me));
    if (card.mode === "react") {
      node.append(reactionPicker(view, card, me));
    } else if (card.mode === "label") {
      node.append(labelForm(view, card));
    } else if (card.mode === "delete") {
      node.append(deleteConfirmation(view, card));
    }
    if (card.error) {
      const box = h("div", { class: "error", role: "alert" });
      showError(box, card.error);
      node.append(box);
    }
    node.append(actions(view, card, mine));
    return node;
  }

  /** Touches up rendered Markdown through the DOM, never by rewriting HTML. */
  function finishBody(body, me) {
    for (const link of body.querySelectorAll("a[href]")) {
      link.target = "_blank";
    }
    // A mention addresses this person when it names their principal, or
    // their principal and the profile they act under.
    const mine = [`@${principalHandle(me)}`.toLowerCase(), `@${me.handle}`.toLowerCase()];
    for (const mention of body.querySelectorAll("span.mention")) {
      if (mine.includes(mention.textContent.toLowerCase())) {
        mention.classList.add("mention-me");
      }
    }
  }

  /** Labels and reactions, in one wrapping row. */
  function chips(view, card, me) {
    const message = card.message;
    const row = h("div", { class: "chips" });
    for (const [key, value] of Object.entries(message.labels || {})) {
      row.append(
        h(
          "span",
          { class: "label" },
          h("span", { class: "label-text", text: `${key}=${value}` }),
          h("button", {
            type: "button",
            class: "label-remove",
            "aria-label": `Remove label ${key}`,
            title: "Remove label",
            dataset: { fk: `${message.id} unlabel ${key}` },
            text: "×",
            on: { click: () => act(view, card, () => api.clearLabel(view.board, message.id, key), `${message.id} label`) },
          }),
        ),
      );
    }
    const myHandle = principalHandle(me);
    for (const reaction of message.reactions || []) {
      const given = reaction.by.includes(myHandle);
      const who = reaction.by.join(", ");
      row.append(
        h(
          "button",
          {
            type: "button",
            class: "reaction",
            "aria-pressed": given ? "true" : "false",
            "aria-label": `${reaction.name}, ${reaction.by.length}, from ${who}. ${given ? "Remove yours" : "Add yours"}`,
            title: `${reaction.name}: ${who}`,
            dataset: { fk: `${message.id} reaction ${reaction.name}` },
            on: { click: () => toggleReaction(view, card, reaction.name, given) },
          },
          h("span", { class: "reaction-char", text: reactionText(reaction.name) }),
          h("span", { class: "reaction-count", text: String(reaction.by.length) }),
          h("span", { class: "reaction-who", text: who }),
        ),
      );
    }
    row.hidden = row.childElementCount === 0;
    return row;
  }

  function actions(view, card, mine) {
    const message = card.message;
    const toggle = (mode, label) =>
      h("button", {
        type: "button",
        "aria-expanded": card.mode === mode ? "true" : "false",
        dataset: { fk: `${message.id} ${mode}` },
        text: label,
        on: {
          click: () => {
            const opening = card.mode !== mode;
            setMode(view, card, opening ? mode : null, opening ? `${message.id} ${mode} first` : `${message.id} ${mode}`);
          },
        },
      });
    return h(
      "div",
      { class: "actions" },
      toggle("react", "React"),
      toggle("label", "Label"),
      h("button", {
        type: "button",
        dataset: { fk: `${message.id} mention` },
        text: "Mention",
        title: `Put @${message.by.handle} in your reply`,
        on: { click: () => insertMention(message.by.handle) },
      }),
      mine && toggle("edit", "Edit"),
      mine && toggle("delete", "Delete"),
    );
  }

  /** Opens or closes a card's inline form and moves focus to `focus`. */
  function setMode(view, card, mode, focus) {
    card.mode = mode;
    card.error = null;
    redraw(view, card);
    focusKey(card.node, focus);
  }

  /**
   * Runs one write on a message, then reloads the thread so the card shows
   * what the board now holds, or the error.
   */
  async function act(view, card, write, focus) {
    try {
      await write();
      card.error = null;
    } catch (error) {
      card.error = error.code === "revision_conflict" ? CONFLICT_ON_DELETE : error;
    }
    card.mode = null;
    await refresh().catch(() => {});
    if (current === view) {
      redraw(view, card);
      focusKey(card.node, focus);
    }
  }

  // ----- Reactions and labels --------------------------------------------

  function toggleReaction(view, card, name, given) {
    const write = given ? api.removeReaction : api.addReaction;
    return act(view, card, () => write(view.board, card.message.id, name), `${card.message.id} reaction ${name}`);
  }

  function reactionPicker(view, card, me) {
    const message = card.message;
    const myHandle = principalHandle(me);
    const given = new Set(
      (message.reactions || []).filter((reaction) => reaction.by.includes(myHandle)).map((reaction) => reaction.name),
    );
    return h(
      "div",
      { class: "picker", role: "group", "aria-label": "Add a reaction" },
      OFFERED_REACTIONS.map((name, index) =>
        h("button", {
          type: "button",
          class: "reaction",
          "aria-pressed": given.has(name) ? "true" : "false",
          "aria-label": given.has(name) ? `Remove your ${name}` : `React with ${name}`,
          title: name,
          dataset: { fk: index === 0 ? `${message.id} react first` : `${message.id} pick ${name}` },
          text: reactionText(name),
          on: { click: () => toggleReaction(view, card, name, given.has(name)) },
        }),
      ),
    );
  }

  function labelForm(view, card) {
    const message = card.message;
    const input = h("input", {
      type: "text",
      required: true,
      autocomplete: "off",
      autocapitalize: "none",
      spellcheck: "false",
      placeholder: "key=value",
      "aria-label": "New label, as key=value",
      dataset: { fk: `${message.id} label first` },
    });
    const errorBox = h("div", { class: "error", role: "alert", hidden: true });
    const add = h("button", { type: "submit", class: "primary", text: "Add label" });
    return h(
      "form",
      {
        class: "inline-form",
        "aria-label": "Add a label",
        on: {
          submit: async (event) => {
            event.preventDefault();
            const text = input.value.trim();
            const split = text.indexOf("=");
            if (split < 1 || split === text.length - 1) {
              showError(errorBox, BAD_LABEL);
              return;
            }
            add.disabled = true;
            try {
              await api.setLabel(view.board, message.id, text.slice(0, split), text.slice(split + 1));
            } catch (error) {
              add.disabled = false;
              showError(errorBox, error);
              return;
            }
            await act(view, card, async () => {}, `${message.id} label`);
          },
        },
      },
      h("div", { class: "inline-row" }, input, add, cancelButton(view, card)),
      errorBox,
    );
  }

  function cancelButton(view, card, key) {
    return h("button", {
      type: "button",
      text: "Cancel",
      dataset: key ? { fk: key } : {},
      on: { click: () => setMode(view, card, null, `${card.message.id} ${card.mode}`) },
    });
  }

  // ----- Edit and delete -------------------------------------------------

  function editForm(view, card) {
    const message = card.message;
    const textarea = h("textarea", {
      rows: "4",
      required: true,
      "aria-label": "Edit your message",
      dataset: { fk: `${message.id} edit first` },
    });
    textarea.value = message.text;
    const errorBox = h("div", { class: "error", role: "alert", hidden: true });
    const save = h("button", { type: "submit", class: "primary", text: "Save" });
    // The revision the person has seen. A live update while the form is
    // open does not move it, so a save cannot silently replace a change
    // they were never shown.
    let seenRevision = message.revision;

    async function submit() {
      if (!textarea.value.trim()) {
        textarea.focus();
        return;
      }
      save.disabled = true;
      try {
        await api.editMessage(view.board, message.id, textarea.value, seenRevision);
      } catch (error) {
        save.disabled = false;
        if (error.code !== "revision_conflict" && error.code !== "message_deleted") {
          showError(errorBox, error);
          return;
        }
        await refresh().catch(() => {});
        if (current !== view) {
          return;
        }
        if (card.message.deleted_at) {
          setMode(view, card, null, null);
        } else {
          // They have now been told, so saving again replaces that version.
          seenRevision = card.message.revision;
          showError(errorBox, CONFLICT_ON_EDIT);
        }
        return;
      }
      await act(view, card, async () => {}, `${message.id} edit`);
    }

    return h(
      "form",
      {
        class: "inline-form",
        "aria-label": "Edit your message",
        on: {
          submit: (event) => {
            event.preventDefault();
            submit();
          },
          keydown: (event) => {
            if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
              event.preventDefault();
              submit();
            }
          },
        },
      },
      textarea,
      errorBox,
      h("div", { class: "inline-row end" }, cancelButton(view, card), save),
    );
  }

  function deleteConfirmation(view, card) {
    const message = card.message;
    return h(
      "div",
      { class: "confirm", role: "group", "aria-label": "Confirm deleting this message" },
      h("p", { text: "Delete this message? Its text is removed from the board for everyone and cannot be restored." }),
      h(
        "div",
        { class: "inline-row end" },
        cancelButton(view, card, `${message.id} delete first`),
        h("button", {
          type: "button",
          class: "danger",
          text: "Delete message",
          on: {
            click: () =>
              act(
                view,
                card,
                // The revision that was on screen when Delete was pressed.
                () => api.deleteMessage(view.board, message.id, message.revision),
                `${message.id} delete`,
              ),
          },
        }),
      ),
    );
  }

  // ----- Mentions and the reply box --------------------------------------

  /** The handle reactions and mentions use for an actor: its principal's. */
  function principalHandle(actor) {
    const suffix = actor.profile ? `/${actor.profile}` : "";
    return suffix && actor.handle.endsWith(suffix) ? actor.handle.slice(0, -suffix.length) : actor.handle;
  }

  /** Puts `@handle` at the caret of the reply box. */
  function insertMention(handle) {
    const box = el.replyText;
    const before = box.value.slice(0, box.selectionStart);
    const after = box.value.slice(box.selectionEnd);
    const gap = before && !/\s$/.test(before) ? " " : "";
    const inserted = `${before}${gap}@${handle} `;
    box.value = inserted + after;
    box.setSelectionRange(inserted.length, inserted.length);
    box.focus();
  }

  /** One button per recent participant, to mention them without typing. */
  function drawMentionRow(view, me) {
    const handles = [];
    const messages = [view.thread.root, ...view.thread.replies];
    for (let index = messages.length - 1; index >= 0 && handles.length < MAX_MENTION_CHIPS; index -= 1) {
      const by = messages[index].by;
      if (by.id !== me.id && !handles.includes(by.handle)) {
        handles.push(by.handle);
      }
    }
    const signature = handles.join("\n");
    if (signature === view.participants) {
      return;
    }
    view.participants = signature;
    keepFocus(el.mentionRow, () => {
      el.mentionRow.replaceChildren(
        h("span", { class: "hint", text: "Mention" }),
        ...handles.map((handle) =>
          h("button", {
            type: "button",
            class: "mention-chip",
            dataset: { fk: `chip ${handle}` },
            text: `@${handle}`,
            on: { click: () => insertMention(handle) },
          }),
        ),
      );
    });
    el.mentionRow.hidden = handles.length === 0;
  }

  bindSendForm({
    form: el.replyForm,
    errorBox: el.replyError,
    values: () => {
      if (!current || !current.thread || !el.replyText.value.trim()) {
        el.replyText.focus();
        return null;
      }
      return { board: current.board, thread_id: current.thread.root.id, text: el.replyText.value };
    },
    send: async (reply, key) => {
      const view = current;
      await api.postMessage(reply.board, { thread_id: reply.thread_id }, reply.text, key);
      if (current !== view) {
        return;
      }
      el.replyText.value = "";
      await refresh().catch(() => {});
      scrollToEnd();
    },
  });

  el.jump.addEventListener("click", () => {
    scrollToEnd();
    el.scroll.focus();
  });
  el.scroll.addEventListener("scroll", () => {
    if (nearBottom()) {
      el.jump.hidden = true;
    }
  });

  return { open, close, refresh, showing };
}
