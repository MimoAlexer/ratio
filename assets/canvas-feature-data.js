// Sources and checkpoints use the board's authenticated, project-scoped API.
function element(tag, text, className) {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
}

function button(text, action, label) {
    const control = element("button", text);
    control.type = "button";
    if (label) control.setAttribute("aria-label", label);
    control.addEventListener("click", action);
    return control;
}

function statusLine() {
    const line = element("p", "", "canvas-feature-status");
    line.setAttribute("role", "status");
    return line;
}

function pathFor(api, suffix) {
    return "/api/canvas/" + encodeURIComponent(api.getBoard().id) + "/" + suffix;
}

async function request(url, options = {}) {
    // A stalled write must release the drawing lock. A timed-out write may still have committed.
    const timeout = options.signal ? null : new AbortController();
    const timer = timeout && setTimeout(() => timeout.abort(), 20000);
    try {
        const response = await fetch(url, {
            ...options,
            signal: options.signal || timeout.signal,
            headers: {Accept: "application/json", ...options.headers},
            credentials: "same-origin",
            cache: "no-store",
            redirect: "error"
        });
        return await readResponse(response);
    } catch (error) {
        if (timeout?.signal.aborted) throw new Error("The server did not confirm the change. Check the latest board before retrying.");
        throw error;
    } finally {
        clearTimeout(timer);
    }
}

async function readResponse(response) {
    if (!response.ok) {
        let message;
        if (response.headers.get("content-type")?.includes("application/json")) {
            const detail = await response.json().catch(() => null);
            if (typeof detail?.error === "string") message = detail.error;
        }
        const fallback = {
            400: "Check the entered values and try again.",
            403: "Editing is unavailable in this workspace.",
            404: "This item is no longer available.",
            409: "The board changed. Keep your edits and reload the latest saved board."
        };
        throw new Error(message || fallback[response.status] || "Request failed. Check the connection and try again.");
    }
    if (response.status === 204) return null;
    if (!response.headers.get("content-type")?.includes("application/json")) {
        throw new Error("Sign in to this workspace and try again.");
    }
    return response.json();
}

function recordType(kind) {
    return kind === "issue" || kind === "note";
}

export function mountSources(api, panel, close) {
    const board = api.getBoard();
    const selected = board.document.nodes.find(node => node.id === api.getSelection());
    if (!selected) {
        panel.append(element("p", "Select a node to link a project issue or note."));
        return;
    }
    const status = statusLine();
    const setSource = source => {
        const node = api.getBoard().document.nodes.find(item => item.id === selected.id);
        if (!node || api.getSelection() !== selected.id || api.getBoard().read_only) {
            status.textContent = "Select an editable node to change its source.";
            return;
        }
        const unchanged = node.source?.id === source?.id && node.source?.kind === source?.kind;
        const changed = api.change(doc => {
            const current = doc.nodes.find(item => item.id === selected.id);
            if (current) current.source = source;
        });
        if (changed === false && !unchanged) {
            status.textContent = "Finish the current save before changing this source.";
            return;
        }
        close();
    };

    if (selected.source && recordType(selected.source.kind)) {
        const {id, kind} = selected.source;
        const actions = element("div", undefined, "canvas-feature-actions");
        const open = element("a", "Open linked " + kind);
        open.href = "/" + (kind === "issue" ? "issues" : "notes") + "/" + encodeURIComponent(id);
        open.addEventListener("click", async event => {
            if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
            event.preventDefault();
            if (await api.flush()) location.assign(open.href);
            else status.textContent = "Save your board changes before opening its source.";
        });
        actions.append(open);
        if (!board.read_only) actions.append(button("Unlink source", () => setSource(null)));
        panel.append(actions);
    }
    if (board.read_only) {
        if (!selected.source) panel.append(element("p", "This node has no linked source."));
        panel.append(status);
        return;
    }

    const searchLabel = element("label", "Search project issues and notes", "canvas-feature-form");
    const search = element("input");
    search.type = "search";
    search.maxLength = 160;
    search.autocomplete = "off";
    searchLabel.append(search);
    const results = element("ul", undefined, "canvas-feature-results");
    results.setAttribute("aria-label", "Matching sources");
    panel.append(searchLabel, results, status);

    let timer;
    let active;
    let disposed = false;
    async function load() {
        active?.abort();
        const controller = new AbortController();
        active = controller;
        const query = new URLSearchParams({q: search.value.trim()});
        try {
            const matches = await request(pathFor(api, "sources?") + query, {signal: controller.signal});
            if (controller.signal.aborted || disposed) return;
            results.replaceChildren();
            for (const item of matches) {
                if (!recordType(item.kind)) continue;
                const row = element("li");
                const pick = button("", () => setSource({kind: item.kind, id: item.id}), "Link " + item.kind + ": " + item.label);
                pick.className = "canvas-feature-row";
                pick.append(element("span", item.label), element("small", item.kind === "issue" ? "Issue" : "Note", "canvas-feature-meta"));
                row.append(pick);
                results.append(row);
            }
            status.textContent = results.children.length ? "" :
                (search.value.trim() ? "No matching issues or notes." : "This project has no issues or notes to link.");
        } catch (error) {
            if (controller.signal.aborted || disposed) return;
            status.textContent = error.message;
            status.append(" ", button("Retry search", load));
        }
    }
    search.addEventListener("input", () => {
        active?.abort();
        clearTimeout(timer);
        results.replaceChildren();
        status.textContent = "";
        timer = setTimeout(load, 150);
    });
    search.addEventListener("keydown", event => {
        if (event.key === "ArrowDown" && results.firstElementChild) {
            event.preventDefault();
            results.querySelector("button").focus();
        }
    });
    results.addEventListener("keydown", event => {
        if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
        const row = event.target.closest("li");
        if (!row) return;
        event.preventDefault();
        const next = event.key === "ArrowDown" ? row.nextElementSibling : row.previousElementSibling;
        if (next) next.querySelector("button").focus();
        else if (event.key === "ArrowUp") search.focus();
    });
    load();
    search.focus();
    return () => { disposed = true; clearTimeout(timer); active?.abort(); };
}

function checkpointDate(iso) {
    const date = new Date(iso);
    return Number.isNaN(date.getTime()) ? "" :
        new Intl.DateTimeFormat(undefined, {dateStyle: "medium", timeStyle: "short"}).format(date);
}

export function mountCheckpoints(api, panel, close) {
    const historyPath = pathFor(api, "checkpoints");
    const readOnly = api.getBoard().read_only;
    const body = element("div");
    const list = element("ul", undefined, "canvas-feature-results");
    list.setAttribute("aria-label", "Saved checkpoints");
    const status = statusLine();
    const confirmation = element("div");
    panel.append(body, confirmation, status);
    let disposed = false;
    let pending = false;
    let loadController;
    let confirmTarget;

    const form = element("form", undefined, "canvas-feature-form");
    const nameLabel = element("label", "Checkpoint name");
    const name = element("input");
    name.type = "text";
    name.maxLength = 80;
    name.required = true;
    name.autocomplete = "off";
    nameLabel.append(name);
    const save = element("button", "Save checkpoint");
    save.type = "submit";
    form.append(nameLabel, save);
    if (!readOnly) body.append(form);
    body.append(list);

    function cancelConfirmation() {
        confirmation.replaceChildren();
        body.hidden = false;
        confirmTarget?.focus();
    }

    async function mutate(suffix, data, complete) {
        if (pending || disposed || api.getBoard().read_only) return;
        pending = true;
        status.textContent = "Saving…";
        panel.setAttribute("aria-busy", "true");
        const controls = [...panel.querySelectorAll("button, input")].filter(control => !control.disabled);
        controls.forEach(control => { control.disabled = true; });
        try {
            await api.withBoardLock(async () => {
                const revision = api.getBoard().revision;
                const result = await request(historyPath + suffix, {
                    method: "POST",
                    headers: {"Content-Type": "application/json"},
                    body: JSON.stringify({...data, csrf: api.csrf, revision})
                });
                await complete(result);
            });
        } catch (error) {
            if (disposed) api.notify(error.message);
            else status.textContent = error.message;
        } finally {
            pending = false;
            controls.forEach(control => { control.disabled = false; });
            if (!disposed) {
                panel.removeAttribute("aria-busy");
                if (!body.hidden && document.activeElement === document.body) name.focus();
            }
        }
    }

    async function refresh() {
        loadController?.abort();
        const controller = new AbortController();
        loadController = controller;
        try {
            const entries = await request(historyPath, {signal: controller.signal});
            if (controller.signal.aborted || disposed) return;
            list.replaceChildren();
            if (!entries.length) list.append(element("li", "No checkpoints yet.", "canvas-feature-meta"));
            for (const entry of entries) {
                const row = element("li", undefined, "canvas-feature-row");
                const title = element("div");
                title.append(element("strong", entry.name));
                const date = checkpointDate(entry.created_at);
                const meta = [entry.automatic ? "Recovery point" : "", date].filter(Boolean).join(" · ");
                title.append(element("small", meta, "canvas-feature-meta"));
                row.append(title);
                if (!readOnly) {
                    const actions = element("div", undefined, "canvas-feature-actions");
                    actions.append(button("Restore", event => ask(entry, false, event.currentTarget), "Restore " + entry.name));
                    if (!entry.automatic) actions.append(button("Delete", event => ask(entry, true, event.currentTarget), "Delete " + entry.name));
                    row.append(actions);
                }
                list.append(row);
            }
        } catch (error) {
            if (controller.signal.aborted || disposed) return;
            status.textContent = error.message;
            status.append(" ", button("Reload checkpoints", () => { status.textContent = ""; refresh(); }));
        }
    }

    function ask(entry, deleting, trigger) {
        if (pending) return;
        confirmTarget = trigger;
        status.textContent = "";
        body.hidden = true;
        const prompt = element("p");
        prompt.append(deleting ? "Delete " : "Restore ", element("strong", entry.name), "?");
        const note = element("p", deleting ? "This permanently removes the saved checkpoint." :
            "Your current drawing will be kept as a recovery point.");
        const actions = element("div", undefined, "canvas-feature-actions");
        const cancel = button("Cancel", cancelConfirmation);
        const action = deleting ? "Delete" : "Restore";
        const accept = button(action + " checkpoint", () => {
            const suffix = "/" + encodeURIComponent(entry.id) + "/" + action.toLowerCase();
            mutate(suffix, {}, result => {
                if (!deleting) api.reloadBoard(result);
                if (disposed) return;
                if (!deleting) {
                    api.notify("Checkpoint restored.");
                    close();
                } else {
                    cancelConfirmation();
                    status.textContent = "Checkpoint deleted.";
                    confirmTarget?.closest("li")?.remove();
                    refresh();
                }
            });
        });
        actions.append(cancel, accept);
        confirmation.replaceChildren(prompt, note, actions);
        cancel.focus();
    }

    form.addEventListener("submit", event => {
        event.preventDefault();
        const checkpointName = name.value.trim();
        if (!checkpointName) {
            name.setCustomValidity("Enter a checkpoint name.");
            name.reportValidity();
            return;
        }
        mutate("", {name: checkpointName}, () => {
            if (disposed) return;
            name.value = "";
            status.textContent = "Checkpoint saved.";
            refresh();
        });
    });
    name.addEventListener("input", () => name.setCustomValidity(""));
    refresh();
    if (!readOnly) name.focus();
    return () => {
        disposed = true;
        loadController?.abort();
        panel.removeAttribute("aria-busy");
    };
}
