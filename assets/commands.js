// The workspace command menu enhances native GET links. Search never submits the underlying page.
(() => {
    "use strict";

    const triggers = [...document.querySelectorAll("a[data-command-trigger]")];
    if (!triggers.length || !window.HTMLDialogElement?.prototype.showModal) return;
    const main = document.querySelector("#main");
    const uuid = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;
    const validId = value => uuid.test(value ?? "") ? value : null;
    const scopeOf = url => validId(/^\/projects\/([^/]+)\/?$/.exec(url.pathname)?.[1])
        || validId(url.searchParams.get("project"));
    const originUrl = new URL(location.href);
    const entityCrumb = main?.querySelector(
        '.breadcrumbs a[href^="/projects/"], [aria-label="Release navigation"] a[href*="project="], header a[href^="/notes?project="], .breadcrumbs a[href*="project="]'
    );
    const pageProject = scopeOf(new URL(triggers[0].href)) || scopeOf(originUrl)
        || validId(main?.querySelector("[data-command-project]")?.dataset.commandProject)
        || validId(main?.querySelector("[data-note-editor]")?.dataset.project)
        || (entityCrumb && scopeOf(new URL(entityCrumb.href)));

    function destination(path, project, fragment = "", text = "") {
        const url = new URL(path, location.origin);
        if (project) url.searchParams.set("project", project);
        if (text) url.searchParams.set("q", text);
        url.hash = fragment;
        return url;
    }

    const dialog = document.createElement("dialog");
    dialog.className = "command-dialog";
    dialog.id = "workspace-commands";
    dialog.setAttribute("aria-label", "Search workspace");
    // User- and server-provided strings are inserted only via DOM text properties below.
    dialog.innerHTML = `
        <div class="command-searchline">
            <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="10" cy="10" r="6"/><path d="m14.5 14.5 5.5 5.5"/></svg>
            <input id="command-input" type="text" enterkeyhint="go" role="combobox" aria-label="Search workspace" aria-autocomplete="list" aria-controls="command-options" aria-expanded="true" placeholder="Search or run a command…" maxlength="120" autocomplete="off" autocapitalize="off" spellcheck="false">
            <button type="button" class="command-close" aria-label="Close search"><span class="command-escape" aria-hidden="true">Esc</span><svg class="command-dismiss" aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="m6 6 12 12M18 6 6 18"/></svg></button>
        </div>
        <div class="command-scope" role="group" aria-label="Search scope" hidden>
            <button type="button" data-scope="project">This project</button>
            <button type="button" data-scope="all">All projects</button>
        </div>
        <div class="command-tray">
            <div id="command-options" role="listbox" aria-label="Commands and search results"></div>
            <p id="command-status" role="status" class="command-feedback"></p>
            <button class="command-retry" type="button" hidden>Try again</button>
        </div>
        <div class="command-foot"><span class="command-hints" aria-hidden="true"><kbd>↑</kbd><kbd>↓</kbd> Move <kbd>↵</kbd> Open</span><span class="command-pending" aria-hidden="true" hidden>Searching…</span>
            <a class="command-full">Open issue search <span aria-hidden="true">→</span></a>
        </div>`;
    document.body.append(dialog);
    const input = dialog.querySelector("#command-input");
    const list = dialog.querySelector("#command-options");
    const tray = dialog.querySelector(".command-tray");
    const status = dialog.querySelector("#command-status");
    const retry = dialog.querySelector(".command-retry");
    const fullSearch = dialog.querySelector(".command-full");
    const hints = dialog.querySelector(".command-hints");
    const pending = dialog.querySelector(".command-pending");
    const scopeBar = dialog.querySelector(".command-scope");
    const projectButton = scopeBar.querySelector('[data-scope="project"]');
    const allButton = scopeBar.querySelector('[data-scope="all"]');
    const close = dialog.querySelector(".command-close");

    const glyph = {
        issue: "M20 12a8 8 0 1 1-16 0 8 8 0 0 1 16 0ZM9 12h6",
        note: "M5 3h10l4 4v14H5zM15 3v5h4M8.5 12h7M8.5 16h5",
        project: "M3 7V5h6l2 2h10v12H3z",
        create: "M12 5v14M5 12h14",
        navigate: "M5 5v6a3 3 0 0 0 3 3h11m-4-4 4 4-4 4",
    };
    const iconNs = "http://www.w3.org/2000/svg";
    function icon(kind) {
        const svg = document.createElementNS(iconNs, "svg");
        svg.setAttribute("aria-hidden", "true");
        svg.setAttribute("viewBox", "0 0 24 24");
        const path = document.createElementNS(iconNs, "path");
        path.setAttribute("d", glyph[kind]);
        svg.append(path);
        return svg;
    }

    let composing = false;
    let project = pageProject;
    let origin;
    let timer;
    let request;
    let version = 0;
    let options = [];
    let active = -1;
    let canCreate = false;
    let currentScopeKnown = false;

    function actions(term) {
        const entries = [];
        function add(title, kind, href, keywords) {
            if (!term || keywords.includes(term)) entries.push({title, kind, href, detail: ""});
        }
        if (currentScopeKnown && canCreate) {
            // Keep current issue-list filters when opening its native composer (including Inbox).
            const issueUrl = originUrl.pathname === "/issues" && project === pageProject
                ? new URL(originUrl) : destination("/issues", project);
            issueUrl.hash = "new-issue";
            add("Create issue", "create", issueUrl.href, "new issue create issue add issue");
            add("Create note", "create", destination("/notes/new", project).href, "new note create note add note markdown");
        }
        if (term) {
            for (const link of document.querySelectorAll(".sidebar a.nav-link")) {
                const title = link.textContent.trim();
                const url = new URL(link.href);
                if (project && ["/", "/issues", "/notes", "/roadmap", "/releases", "/insights", "/health"].includes(url.pathname)) {
                    url.searchParams.set("project", project);
                }
                add("Go to " + title, "navigate", url.href, "go to " + title.toLowerCase());
            }
        } else if (currentScopeKnown && !canCreate) {
            add("Open projects", "navigate", "/projects", "");
        }
        return entries;
    }

    function validResult(entry) {
        const prefix = {issue: "issues", note: "notes", project: "projects"}[entry?.kind];
        return prefix && uuid.test(entry.id) && entry.href === "/" + prefix + "/" + entry.id
            && typeof entry.title === "string" && typeof entry.detail === "string";
    }

    function select(index, scroll = false) {
        if (active >= 0 && options[active]) options[active].setAttribute("aria-selected", "false");
        active = index;
        if (active < 0) {
            input.removeAttribute("aria-activedescendant");
            return;
        }
        const item = options[index];
        item.setAttribute("aria-selected", "true");
        input.setAttribute("aria-activedescendant", item.id);
        if (scroll) item.scrollIntoView({block: "nearest"});
    }

    function followLink(event) {
        if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
        origin = null;
        // A same-fragment navigation does not emit hashchange. Use the shell's
        // regular destination handler after the modal has restored focus.
        if (event.currentTarget.href === location.href && location.pathname === "/issues"
            && ["#search", "#new-issue"].includes(location.hash)) {
            event.preventDefault();
            dialog.addEventListener("close", () => {
                window.dispatchEvent(new Event("ratio:focus-destination"));
            }, {once: true});
        }
        dialog.close();
    }

    function group(label, entries) {
        if (!entries.length) return;
        const section = document.createElement("div");
        section.setAttribute("role", "group");
        section.setAttribute("aria-label", label);
        const heading = document.createElement("div");
        heading.className = "command-group-title";
        heading.setAttribute("aria-hidden", "true");
        heading.textContent = label;
        section.append(heading);
        for (const entry of entries) {
            const link = document.createElement("a");
            link.className = "command-option";
            link.href = entry.href;
            link.tabIndex = -1;
            link.setAttribute("role", "option");
            link.setAttribute("aria-selected", "false");
            link.id = "command-choice-" + options.length;
            link.title = [entry.title, entry.detail].filter(Boolean).join(" · ");
            const title = document.createElement("span");
            title.className = "command-option-title";
            title.textContent = entry.title;
            link.append(icon(entry.kind), title);
            if (entry.detail) {
                const detail = document.createElement("span");
                detail.className = "command-detail";
                detail.textContent = entry.detail;
                link.setAttribute("aria-label", entry.kind + ", " + entry.title + ", " + entry.detail);
                link.append(detail);
            }
            const index = options.length;
            link.addEventListener("pointermove", event => {
                if (event.pointerType === "mouse" && index !== active) select(index);
            });
            link.addEventListener("mousedown", event => {
                if (event.button === 0) event.preventDefault(); // Keep text editing focus in the combobox.
            });
            link.addEventListener("click", followLink);
            options.push(link);
            section.append(link);
        }
        list.append(section);
    }

    function render(results, phase) {
        select(-1);
        options = [];
        const query = input.value.trim();
        // During a new query, keep the preceding list's geometry until the
        // response arrives. Inert rows cannot navigate to an obsolete match.
        const holdRows = phase === "loading" && list.childElementCount > 0;
        list.inert = holdRows;
        list.setAttribute("aria-busy", String(phase === "loading"));
        if (!holdRows) list.replaceChildren();
        const commandEntries = actions(query.toLowerCase());
        if (phase === "ready") {
            if (query && commandEntries.some(row => row.kind === "create" && row.title.toLowerCase() === query.toLowerCase())) {
                group("Actions", commandEntries);
                group("Matches", results);
            } else {
                group(query ? "Matches" : "Recent work", results);
                group("Actions", commandEntries);
            }
        }
        if (phase === "error") group("Navigation", commandEntries.filter(row => row.kind === "navigate"));
        select(options.length ? 0 : -1);
        tray.scrollTop = 0;
        hints.hidden = options.length === 0;
        pending.hidden = !holdRows;
        retry.hidden = phase !== "error";
        let message;
        if (phase === "loading") message = "Searching…";
        else if (phase === "error") message = "Workspace search is unavailable.";
        else if (!options.length) message = "No matching issues, notes, projects or actions.";
        else if (!query && !results.length) message = "No recent work.";
        else message = options.length + (options.length === 1 ? " option available." : " options available.");
        status.classList.toggle("command-sr", holdRows || phase === "ready" && (results.length > 0 || (query && options.length > 0)));
        status.textContent = message;
        const searchPage = destination("/issues", project, "search", query);
        if (query) searchPage.searchParams.set("scope", "all");
        fullSearch.href = searchPage;
    }

    function search(delay = 0) {
        clearTimeout(timer);
        request?.abort();
        const ticket = ++version;
        render([], "loading");
        timer = setTimeout(async () => {
            const controller = new AbortController();
            request = controller;
            try {
                const response = await fetch(destination("/api/commands", project, "", input.value.trim()), {
                    credentials: "same-origin", cache: "no-store", redirect: "error",
                    headers: {Accept: "application/json"}, signal: controller.signal,
                });
                if (!response.ok) throw new Error();
                const payload = await response.json();
                if (ticket !== version || !dialog.open) return;
                if (!Array.isArray(payload.results) || typeof payload.can_create !== "boolean"
                    || (project && (payload.scope?.id !== project || typeof payload.scope.name !== "string"))) throw new Error();
                currentScopeKnown = true;
                canCreate = payload.can_create;
                if (project) {
                    projectButton.textContent = payload.scope.name + (payload.scope.archived ? " (archived)" : "");
                    projectButton.title = payload.scope.name;
                }
                render(payload.results.filter(validResult), "ready");
            } catch {
                if (ticket === version && dialog.open && !controller.signal.aborted) render([], "error");
            }
        }, delay);
    }

    function chooseScope(next) {
        project = next;
        currentScopeKnown = false;
        scopeBar.hidden = !pageProject;
        projectButton.setAttribute("aria-pressed", String(Boolean(project)));
        allButton.setAttribute("aria-pressed", String(!project));
        input.focus({preventScroll: true});
        search();
    }
    projectButton.addEventListener("click", () => chooseScope(pageProject));
    allButton.addEventListener("click", () => chooseScope(null));
    close.addEventListener("click", () => dialog.close());
    retry.addEventListener("click", () => {
        input.focus({preventScroll: true});
        search();
    });
    let startedOnBackdrop = false;
    dialog.addEventListener("pointerdown", event => { startedOnBackdrop = event.target === dialog; });
    dialog.addEventListener("click", event => {
        if (event.target === dialog && startedOnBackdrop) dialog.close();
    });
    dialog.addEventListener("close", () => {
        ++version;
        clearTimeout(timer);
        request?.abort();
        input.value = "";
        list.replaceChildren();
        options = [];
        if (origin?.isConnected) origin.focus({preventScroll: true});
    });
    fullSearch.addEventListener("click", followLink);
    input.addEventListener("compositionstart", () => { composing = true; });
    input.addEventListener("compositionend", () => {
        composing = false;
        search(100);
    });
    dialog.addEventListener("cancel", event => {
        if (composing) event.preventDefault();
    });
    input.addEventListener("input", event => { if (!event.isComposing) search(100); });
    input.addEventListener("keydown", event => {
        if (event.isComposing || event.keyCode === 229 || event.altKey || event.metaKey || event.ctrlKey) return;
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            if (!options.length) return;
            select((active + (event.key === "ArrowDown" ? 1 : -1) + options.length) % options.length, true);
        } else if (event.key === "Enter") {
            event.preventDefault();
            options[active]?.click();
        }
    });

    function openMenu(source) {
        if (dialog.open || document.querySelector("dialog[open]")) return;
        origin = source;
        composing = false;
        input.value = "";
        projectButton.textContent = "This project";
        dialog.showModal();
        chooseScope(pageProject);
    }
    for (const trigger of triggers) {
        trigger.href = destination("/issues", pageProject, "search");
        const label = trigger.querySelector("[data-command-label]");
        if (label) label.textContent = "Search";
        const key = trigger.querySelector("[data-command-key]");
        if (key) {
            key.hidden = false;
            key.textContent = /Mac|iPhone|iPad/i.test(navigator.platform) ? "⌘ K" : "Ctrl K";
            key.setAttribute("aria-hidden", "true");
        }
        trigger.setAttribute("aria-label", "Search workspace");
        trigger.setAttribute("title", "Search workspace");
        trigger.setAttribute("aria-keyshortcuts", "Control+K Meta+K");
        trigger.setAttribute("aria-haspopup", "dialog");
        trigger.setAttribute("aria-controls", dialog.id);
        trigger.addEventListener("click", event => {
            if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
            event.preventDefault();
            openMenu(trigger);
        });
    }
    document.addEventListener("keydown", event => {
        if (event.defaultPrevented || event.repeat || event.isComposing || event.keyCode === 229
            || event.altKey || event.shiftKey || event.metaKey === event.ctrlKey || event.key.toLowerCase() !== "k"
            || document.querySelector("dialog[open]")) return;
        if (event.composedPath().some(node => node instanceof HTMLElement && (node.isContentEditable
            || node.matches('input, textarea, select, [role="textbox"], [role="searchbox"], [role="combobox"]')))) return;
        event.preventDefault();
        openMenu(document.activeElement);
    });
})();
