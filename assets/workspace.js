// Native links and forms remain the destinations for every shortcut.
(() => {
    "use strict";

    const projectId = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;
    const preference = "workspace.single-key-shortcuts";
    let singleKeys = true;
    try {
        singleKeys = localStorage.getItem(preference) !== "off";
    } catch {
        // The preference still works for this page if browser storage is unavailable.
    }

    function currentProject() {
        const url = new URL(location.href);
        const fromPath = /^\/projects\/([^/]+)\/?$/.exec(url.pathname)?.[1];
        if (projectId.test(fromPath ?? "")) return fromPath;

        const filter = url.searchParams.get("project");
        if (projectId.test(filter ?? "")) return filter;

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
            dialog.className = "panel";
            dialog.setAttribute("aria-labelledby", "workspace-shortcut-title");
            dialog.innerHTML = `
                <h2 id="workspace-shortcut-title">Keyboard shortcuts</h2>
                <dl>
                    <dt><kbd>Ctrl</kbd> + <kbd>K</kbd> or <kbd>⌘</kbd> + <kbd>K</kbd></dt><dd>Search issues</dd>
                    <dt><kbd>/</kbd></dt><dd>Search issues</dd>
                    <dt><kbd>C</kbd></dt><dd>Create an issue</dd>
                    <dt><kbd>?</kbd></dt><dd>Show keyboard shortcuts</dd>
                    <dt><kbd>Esc</kbd></dt><dd>Close this dialog</dd>
                </dl>
                <label><input type="checkbox" name="single_keys"> Enable single-key shortcuts</label>
                <form method="dialog"><button type="submit" autofocus>Close</button></form>`;
            const toggle = dialog.querySelector("input");
            toggle.checked = singleKeys;
            toggle.addEventListener("change", () => {
                singleKeys = toggle.checked;
                try {
                    localStorage.setItem(preference, singleKeys ? "on" : "off");
                } catch {
                    // This page keeps the choice in memory.
                }
            });
            document.body.append(dialog);
        }
        if (!dialog.open) {
            const origin = document.activeElement;
            dialog.addEventListener("close", () => {
                if (origin instanceof HTMLElement && origin.isConnected) origin.focus({ preventScroll: true });
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

        if (event.ctrlKey || event.metaKey) {
            if (event.ctrlKey !== event.metaKey && !event.shiftKey && event.key.toLowerCase() === "k") {
                event.preventDefault();
                goToIssues("search");
            }
            return;
        }
        if (!singleKeys) return;
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

    window.addEventListener("hashchange", focusDestination);
    // On full navigation the browser focuses the fragment after deferred scripts run.
    window.addEventListener("pageshow", event => {
        if (!event.persisted) requestAnimationFrame(focusDestination);
    });
})();
