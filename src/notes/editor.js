// Markdown editing and recovery are local to this tab. The server validates
// revisions and handles every submission, including submissions without JS.
(() => {
  "use strict";

  const drafts = "workspace:note-draft-v1:";
  const epochs = "workspace:note-draft-epoch-v1:";
  const changes = new EventTarget();
  let channel;
  try {
    channel = new BroadcastChannel("workspace-note-drafts-v1");
  } catch {
    // The durable account epoch also propagates through storage events.
  }

  function draftKey(user, project, note) {
    return drafts + [user, project, note || "new"].join(":");
  }

  function generation(kind) {
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    return kind + Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");
  }

  function accountEpoch(user, initialize = false) {
    try {
      const key = epochs + user;
      if (initialize && localStorage.getItem(key) === null) {
        // Reserve the epoch before retaining any text. Following eviction or
        // deletion, a new generation cannot match an older tab's draft.
        localStorage.setItem(key, generation("initial-"));
      }
      return localStorage.getItem(key);
    } catch {
      return null;
    }
  }

  function readDraft(key, epoch) {
    try {
      const draft = JSON.parse(sessionStorage.getItem(key));
      const legacy = draft && draft.epoch === undefined && epoch?.startsWith("initial-");
      if (draft && epoch !== null && (legacy || draft.epoch === epoch) &&
          ["title", "path", "body", "revision"].every(k => typeof draft[k] === "string")) {
        return draft;
      }
    } catch {
      // Browsers with storage disabled can still use the editor.
    }
    return null;
  }

  function removeDraft(key) {
    try {
      sessionStorage.removeItem(key);
    } catch {
      // Recovery also checks the account epoch before reading a draft.
    }
  }

  function purgeObsolete(user, epoch) {
    try {
      const prefix = drafts + user + ":";
      for (const key of Object.keys(sessionStorage)) {
        if (key.startsWith(prefix) && (epoch === null || !readDraft(key, epoch))) removeDraft(key);
      }
    } catch {
      // Storage may be disabled in this tab.
    }
  }

  function receive(change) {
    if (!change || typeof change.user !== "string" || !change.user) return;
    if (change.type === "logout") {
      const current = accountEpoch(change.user);
      if (current !== null && change.epoch !== null && current !== change.epoch) return;
      if (current !== null && change.epoch === null && change.previous && current !== change.previous) return;
      purgeObsolete(change.user, current === null ? null : change.epoch);
    } else if (change.type === "delete" && change.project && change.note) {
      removeDraft(draftKey(change.user, change.project, change.note));
    } else {
      return;
    }
    changes.dispatchEvent(new CustomEvent("cleanup", {detail: change}));
  }

  function announce(change) {
    receive(change);
    channel?.postMessage(change);
  }

  function clearAccount(user) {
    const previous = accountEpoch(user);
    let epoch = generation("cleared-");
    try {
      localStorage.setItem(epochs + user, epoch);
    } catch {
      // Removing a previously allocated entry is still possible at quota.
      try { localStorage.removeItem(epochs + user); } catch { /* Storage denied. */ }
      epoch = null;
    }
    announce({type: "logout", user, epoch, previous});
  }

  channel?.addEventListener("message", event => receive(event.data));
  window.addEventListener("storage", event => {
    if (event.key?.startsWith(epochs) && (event.newValue === null || event.newValue.startsWith("cleared-"))) {
      receive({type: "logout", user: event.key.slice(epochs.length), epoch: event.newValue, previous: event.oldValue});
    }
  });

  function acknowledge(element) {
    const {user, project, note, origin, draftToken} = element.dataset;
    if (!user || !project || !note || !draftToken || !["created", "updated"].includes(origin)) return;
    const key = draftKey(user, project, origin === "created" ? "" : note);
    if (readDraft(key, accountEpoch(user))?.pending !== draftToken) return;
    try {
      sessionStorage.removeItem(key);
    } catch {
      // If the browser refuses deletion, the draft remains recoverable.
    }
  }

  function sameDraft(left, right) {
    return ["title", "path", "body", "revision"].every(k => left[k] === right[k]);
  }

  function searchLinks(input, source) {
    const list = input.list;
    if (!list || !source || typeof fetch !== "function") return;
    const endpoint = new URL(source, document.baseURI);
    if (endpoint.origin !== location.origin) return;
    const initial = Array.from(list.options);
    let timer;
    let request;
    let version = 0;

    async function load(query, versionAtStart) {
      const url = new URL(endpoint);
      url.searchParams.set("q", query);
      const controller = new AbortController();
      request = controller;
      try {
        const response = await fetch(url, {
          headers: {Accept: "application/json"},
          mode: "same-origin", signal: controller.signal
        });
        if (!response.ok) return;
        const items = await response.json();
        if (versionAtStart !== version || input.value !== query || !Array.isArray(items)) return;
        const options = [];
        for (const item of items) {
          if (typeof item?.token === "string" && typeof item.label === "string") {
            options.push(new Option(item.label, item.token));
          }
        }
        list.replaceChildren(...options);
      } catch {
        // The current list remains available during interrupted requests.
      } finally {
        if (request === controller) request = undefined;
      }
    }

    input.addEventListener("input", () => {
      clearTimeout(timer);
      request?.abort();
      const currentVersion = ++version;
      const query = input.value;
      if (!query.trim()) {
        list.replaceChildren(...initial);
        return;
      }
      // Native datalists dispatch input when a choice is accepted. Keep that
      // option available until the editor's Insert button consumes it.
      if (Array.from(list.options).some(option => option.value === query)) return;
      const original = initial.find(option => option.value === query);
      if (original) {
        list.append(original);
        return;
      }
      timer = setTimeout(() => load(query, currentVersion), 200);
    });
  }

  function insertLinks(form, body) {
    const tools = form.querySelector("[data-link-tools]");
    const input = tools?.querySelector("[data-link-input]");
    const insert = tools?.querySelector("[data-insert-link]");
    if (!input || !insert) return;
    searchLinks(input, tools.dataset.suggestionsUrl);
    tools.hidden = false;
    insert.addEventListener("click", event => {
      event.preventDefault();
      const token = input.value;
      if (!token || !input.list || !Array.from(input.list.options).some(option => option.value === token)) {
        input.focus();
        return;
      }
      body.setRangeText(token, body.selectionStart, body.selectionEnd, "end");
      body.focus();
      body.dispatchEvent(new Event("input", {bubbles: true}));
      input.value = "";
      input.dispatchEvent(new Event("input"));
    });
  }

  function enhance(form) {
    const {user, project, note} = form.dataset;
    const fields = form.elements;
    const title = fields.namedItem("title");
    const path = fields.namedItem("path");
    const body = fields.namedItem("body");
    const revision = fields.namedItem("revision");
    if (!user || !project || !title || !path || !body) return;
    insertLinks(form, body);
    const actions = form.querySelector("[data-draft-actions]");
    const status = form.querySelector("[data-draft-status]");
    const restore = form.querySelector("[data-restore]");
    const discard = form.querySelector("[data-discard]");
    if (!actions || !status || !restore || !discard) return;

    const key = draftKey(user, project, note);
    const snapshot = () => ({
      title: title.value, path: path.value, body: body.value,
      revision: revision?.value ?? form.dataset.revision ?? ""
    });
    const initial = snapshot();
    const epoch = accountEpoch(user, true);
    if (epoch !== null) purgeObsolete(user, epoch);
    let recovery = readDraft(key, epoch);
    let revoked = false;
    let dirty = false;
    let timer;

    function revoke() {
      revoked = true;
      clearTimeout(timer);
      dirty = false;
      resetActions();
      status.textContent = "Local drafts were cleared.";
    }

    function current() {
      const live = accountEpoch(user);
      if (!revoked && live !== epoch) {
        purgeObsolete(user, live);
        revoke();
      }
      return !revoked;
    }

    changes.addEventListener("cleanup", event => {
      const change = event.detail;
      if (change.user !== user) return;
      const ownerCleared = change.type === "logout" && (change.epoch === null || change.epoch !== epoch);
      const noteDeleted = change.type === "delete" && change.project === project && change.note === note;
      if (ownerCleared || noteDeleted) revoke();
    });

    function write(pending = null) {
      clearTimeout(timer);
      if (!current()) return;
      try {
        if (epoch === null) throw new Error("Draft storage is unavailable");
        sessionStorage.setItem(key, JSON.stringify({...snapshot(), epoch, pending}));
        dirty = false;
        status.textContent = "Draft kept in this tab.";
      } catch {
        status.textContent = "This tab could not keep a draft. Save the note or copy your text before leaving.";
      }
    }

    function flush() {
      if (dirty) write();
    }

    function resetActions() {
      recovery = null;
      actions.hidden = true;
    }

    if (recovery && !sameDraft(recovery, initial)) {
      actions.hidden = false;
      status.textContent = "An earlier draft is available in this tab.";
    }

    restore.addEventListener("click", event => {
      event.preventDefault();
      if (!current() || !recovery) return;
      title.value = recovery.title;
      path.value = recovery.path;
      body.value = recovery.body;
      if (revision) revision.value = recovery.revision;
      const stale = initial.revision !== recovery.revision;
      resetActions();
      status.textContent = stale
        ? "Restored an earlier revision. Saving will check for changes to the note."
        : "Draft restored.";
      body.focus();
    });

    discard.addEventListener("click", event => {
      event.preventDefault();
      if (!current()) return;
      clearTimeout(timer);
      try {
        sessionStorage.removeItem(key);
        dirty = false;
        resetActions();
        status.textContent = "Draft discarded.";
      } catch {
        status.textContent = "This tab could not discard the draft.";
      }
    });

    form.addEventListener("input", event => {
      if (![title, path, body].includes(event.target) || !current()) return;
      resetActions();
      dirty = true;
      clearTimeout(timer);
      timer = setTimeout(flush, 250);
    });
    window.addEventListener("pagehide", flush);
    window.addEventListener("pageshow", current);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "hidden") flush();
    });

    form.addEventListener("submit", event => {
      const action = event.submitter?.getAttribute("formaction");
      if (action && new URL(action, document.baseURI).pathname.endsWith("/preview")) {
        write();
        return;
      }
      // Only the successful redirect for this exact submission can acknowledge
      // it. Revisiting an older success URL leaves subsequently edited text alone.
      const token = crypto.randomUUID?.();
      if (!token) {
        write();
        return;
      }
      let hidden = fields.namedItem("draft_token");
      if (!hidden) {
        hidden = document.createElement("input");
        hidden.type = "hidden";
        hidden.name = "draft_token";
        form.append(hidden);
      }
      hidden.value = token;
      write(token);
    });
  }

  function start() {
    document.querySelectorAll("script[data-clear-note-drafts]").forEach(script => {
      const user = script.dataset.clearNoteDrafts;
      if (user) clearAccount(user);
    });
    document.querySelectorAll("[data-note-deleted]").forEach(marker => {
      const {user, project, note} = marker.dataset;
      if (user && project && note) announce({type: "delete", user, project, note});
    });
    document.querySelectorAll("[data-note-saved]").forEach(acknowledge);
    document.querySelectorAll("form[data-note-editor]").forEach(enhance);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, {once: true});
  } else {
    start();
  }
})();
