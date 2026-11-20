// Native links and forms remain the destinations for every shortcut.
(() => {
    "use strict";

    const projectId = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;
    function readPreference(key, previousKey) {
        let value = null;
        try {
            value = localStorage.getItem(key);
            if (value === null) {
                value = localStorage.getItem(previousKey);
                if (value !== null) localStorage.setItem(key, value);
            }
        } catch { /* Preferences still work on this page when storage is unavailable. */ }
        return value;
    }

    const shortcutKey = "ratio.single-key-shortcuts";
    let singleKeys = readPreference(shortcutKey, "workspace.single-key-shortcuts") !== "off";
    const themeKey = "ratio.display";
    function applyTheme(mono) {
        document.documentElement.dataset.theme = mono ? "mono" : "graphite";
        for (const button of document.querySelectorAll("[data-theme-toggle]")) {
            button.hidden = false;
            button.setAttribute("aria-pressed", String(mono));
            button.title = mono ? "Use graphite theme" : "Use black and white theme";
        }
    }
    applyTheme(readPreference(themeKey, "working-set.display") === "mono");
    for (const button of document.querySelectorAll("[data-theme-toggle]")) {
        button.addEventListener("click", () => {
            const mono = document.documentElement.dataset.theme !== "mono";
            applyTheme(mono);
            try { localStorage.setItem(themeKey, mono ? "mono" : "graphite"); } catch {}
        });
    }

    if (!document.querySelector(".app-shell")) return;

    async function loadProjectLinks() {
        const slots = [...document.querySelectorAll("[data-sidebar-projects]")];
        if (!slots.length) return;
        try {
            const response = await fetch("/api/sidebar/projects", {headers: {Accept:"application/json"}, credentials:"same-origin", cache:"no-store", redirect:"error"});
            if (!response.ok || !response.headers.get("content-type")?.includes("application/json")) return;
            const items = await response.json();
            if (!Array.isArray(items)) return;
            for (const slot of slots) {
                const links = [];
                for (const item of items.slice(0,6)) {
                    if (!projectId.test(item.id) || typeof item.name !== "string") continue;
                    const link = document.createElement("a");
                    link.href = "/projects/" + item.id;
                    link.textContent = item.name;
                    link.title = item.name;
                    if (currentProject() === item.id) link.setAttribute("aria-current","location");
                    links.push(link);
                }
                slot.replaceChildren(...links);
                slot.hidden = links.length === 0;
            }
        } catch { /* The Projects page is always available without enhancement. */ }
    }
    loadProjectLinks();

    const floatingMenus=".issue-menu[open],.mobile-menu[open],.account-menu[open],[data-disclosure-menu][open],.issue-move[open],.issue-status-picker[open]";
    function visibleSummary(disclosure) {
        const closedParent=disclosure.parentElement?.closest("details:not([open])");
        return (closedParent || disclosure).querySelector(":scope > summary");
    }
    function closeAwayFrom(target) {
        for(const menu of document.querySelectorAll(floatingMenus)) {
            if(!menu.contains(target)) menu.open=false;
        }
    }
    document.addEventListener("keydown", event => {
        if(event.key!=="Escape"||event.defaultPrevented||document.querySelector("dialog[open]")) return;
        const menu=event.target.closest?.(floatingMenus+",.nav-manage[open]");
        if(menu) {
            event.preventDefault();
            menu.open=false;
            visibleSummary(menu)?.focus({preventScroll:true});
        }
    });
    document.addEventListener("pointerdown",event=>closeAwayFrom(event.target));
    // A native disclosure is not modal. When focus leaves it, reveal the newly focused control.
    document.addEventListener("focusin",event=>closeAwayFrom(event.target));

    function currentProject() {
        const url = new URL(location.href);
        const fromPath = /^\/projects\/([^/]+)\/?$/.exec(url.pathname)?.[1];
        if (projectId.test(fromPath ?? "")) return fromPath;

        const filter = url.searchParams.get("project");
        if (projectId.test(filter ?? "")) return filter;

        const scoped=document.querySelector("[data-command-project]")?.dataset.commandProject;
        if(projectId.test(scoped??""))return scoped;
        const breadcrumb = document.querySelector('.breadcrumbs a[href^="/projects/"]');
        const fromBreadcrumb = breadcrumb?.getAttribute("href")?.slice("/projects/".length);
        return projectId.test(fromBreadcrumb ?? "") ? fromBreadcrumb : null;
    }

    function focusDestination() {
        const key = location.pathname + location.hash;
        const selector = {
            "/issues#search": '#search input[name="q"]',
            "/issues#new-issue": '#new-issue input[name="title"]',
            "/projects#new-project": '#new-project input[name="name"]',
        }[key];
        if (!selector) return;
        const field = document.querySelector(selector);
        if (field) {
            for (let details = field.closest("details"); details; details = details.parentElement?.closest("details")) details.open = true;
            field.focus();
        } else if (key === "/issues#new-issue") {
            location.replace("/projects#new-project");
        }
    }

    function goToIssues(fragment) {
        const url = new URL("/issues", location.origin);
        if (location.pathname === url.pathname) {
            url.search = location.search;
        } else {
            const project = currentProject();
            if (project) url.searchParams.set("project", project);
        }
        url.hash = fragment;
        if (url.href === location.href) {
            focusDestination();
        } else {
            location.assign(url);
        }
    }

    let dialog;
    function openHelp() {
        if (!dialog) {
            dialog = document.createElement("dialog");
            dialog.className = "shortcut-dialog";
            dialog.setAttribute("aria-labelledby", "workspace-shortcut-title");
            const onCanvas = Boolean(document.querySelector("[data-canvas-editor]"));
            const localKeys = onCanvas ? `
                    <dt><kbd>[</kbd> / <kbd>]</kbd></dt><dd>Previous or next object</dd>
                    <dt><kbd>Arrows</kbd></dt><dd>Move the selected shape</dd>
                    <dt><kbd>Enter</kbd></dt><dd>Place a shape or edit the selection</dd>
                    <dt><kbd>V</kbd> / <kbd>H</kbd></dt><dd>Select or pan</dd>
                    <dt><kbd>N</kbd> / <kbd>P</kbd> / <kbd>C</kbd></dt><dd>Note, pen or connector</dd>
                    <dt><kbd>Delete</kbd></dt><dd>Remove the selection</dd>
                    <dt><kbd>Ctrl/⌘ Z</kbd> / <kbd>Ctrl/⌘ Shift Z</kbd></dt><dd>Undo or redo</dd>` : `
                    <dt><kbd>/</kbd></dt><dd>Search issues</dd>
                    <dt><kbd>C</kbd></dt><dd>Create an issue</dd>`;
            dialog.innerHTML = `
                <h2 id="workspace-shortcut-title">Keyboard shortcuts</h2>
                ${onCanvas ? "<p>Focus the drawing to use its tools and move between objects.</p>" : ""}
                <dl>
                    <dt><kbd>Ctrl</kbd> + <kbd>K</kbd> or <kbd>⌘</kbd> + <kbd>K</kbd></dt><dd>Search and jump to work</dd>
                    ${localKeys}
                    <dt><kbd>?</kbd></dt><dd>Show keyboard shortcuts</dd>
                    <dt><kbd>Esc</kbd></dt><dd>Close this dialog</dd>
                </dl>
                <label><input type="checkbox" name="single_keys"> ${onCanvas ? "Enable single-key shortcuts outside the drawing" : "Enable single-key shortcuts"}</label>
                <form method="dialog"><button type="submit" autofocus>Close</button></form>`;
            if (document.querySelector(".issue-ledger")) {
                const list=dialog.querySelector("dl");
                for (const [keys, action] of [["↑ / ↓","Move between focused issues"],["Space","Inspect the focused issue"]]) {
                    const term=document.createElement("dt"), key=document.createElement("kbd"), description=document.createElement("dd");
                    key.textContent=keys; term.append(key);description.textContent=action;list.append(term,description);
                }
            }
            const toggle = dialog.querySelector("input");
            toggle.checked = singleKeys;
            toggle.addEventListener("change", () => {
                singleKeys = toggle.checked;
                try {
                    localStorage.setItem(shortcutKey, singleKeys ? "on" : "off");
                } catch {
                    // This page keeps the choice in memory.
                }
            });
            document.body.append(dialog);
        }
        if (!dialog.open) {
            const origin = document.activeElement;
            dialog.addEventListener("close", () => {
                if (origin instanceof HTMLElement && origin.isConnected) {
                    const hiddenMenu=origin.closest("details:not([open])");
                    (hiddenMenu ? visibleSummary(hiddenMenu) : origin)?.focus({preventScroll:true});
                }
            }, { once: true });
            dialog.showModal();
        }
    }

    for (const button of document.querySelectorAll("[data-shortcuts-help]")) {
        button.addEventListener("click", openHelp);
        button.hidden = false;
    }

    document.addEventListener("keydown", event => {
        if (event.defaultPrevented || event.repeat || event.isComposing || event.keyCode === 229 ||
            event.altKey || document.querySelector("dialog[open]")) return;
        if (event.composedPath().some(node => node instanceof HTMLElement &&
            (node.isContentEditable || node.matches('input, textarea, select, [role="textbox"], [role="searchbox"], [role="combobox"]')))) return;

        // The command dialog owns the modified K shortcut; / keeps inline issue search.
        if (event.ctrlKey || event.metaKey) return;
        if (!singleKeys || event.target.closest?.("[data-canvas-editor]")) return;
        if (event.key === "?") {
            event.preventDefault();
            openHelp();
        } else if (event.key === "/") {
            event.preventDefault();
            goToIssues("search");
        } else if (event.key.toLowerCase() === "c" && !event.shiftKey) {
            event.preventDefault();
            goToIssues("new-issue");
        }
    });

    window.addEventListener("ratio:focus-destination", focusDestination);
    window.addEventListener("hashchange", focusDestination);
    // On full navigation the browser focuses the fragment after deferred scripts run.
    window.addEventListener("pageshow", event => {
        if (!event.persisted) requestAnimationFrame(() => requestAnimationFrame(focusDestination));
    });
})();
