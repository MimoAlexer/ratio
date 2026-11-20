// Each preview link also opens a server-rendered view with JavaScript disabled.
(() => {
    "use strict";
    const searchToggle = document.getElementById("issue-search-toggle");
    const inlineSearch = document.querySelector(".issue-query");
    if (searchToggle && inlineSearch) {
        // Native fragments open the search with or without scripting. On a phone
        // keep the toolbar in place when a nearby target already fits onscreen.
        document.addEventListener("click", event => {
            if (event.button !== 0 || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.defaultPrevented) return;
            const link = event.target.closest?.('.issue-search-trigger, .issue-search-close[href="#issue-search-toggle"]');
            if (!link) return;
            const x = window.scrollX, y = window.scrollY;
            const field = link === searchToggle ? inlineSearch.querySelector('input[name="q"]') : searchToggle;
            requestAnimationFrame(() => {
                if (location.hash !== link.hash || !field.getClientRects().length) return;
                field.focus({preventScroll: true});
                window.scrollTo({left: x, top: y, behavior: "instant"});
                if (field.getBoundingClientRect().bottom > window.innerHeight) field.scrollIntoView({block: "nearest"});
            });
        });
    }
    if (!document.querySelector(".issue-ledger")) return;

    const current = () => document.querySelector(".issue-workbench");
    function focusRow(id) {
        const row = document.getElementById("record-" + id);
        if (!row) return;
        const group = row.closest("details");
        if (group) group.open = true;
        row.focus({preventScroll: true});
        row.scrollIntoView({block: "nearest"});
    }

    let request;
    let renderedSearch = location.search;
    async function inspect(link) {
        const workbench = current();
        if (!workbench) return;
        const startUrl = location.href;
        const startFocus = document.activeElement;
        const url = new URL(link.href);
        request?.abort();
        const controller = new AbortController();
        request = controller;
        workbench.setAttribute("aria-busy", "true");
        try {
            const response = await fetch(url, {
                headers: {Accept: "text/html"}, credentials: "same-origin",
                cache: "no-store", signal: controller.signal
            });
            if (response.redirected || !response.ok) throw new Error("Navigation required");
            const page = new DOMParser().parseFromString(await response.text(), "text/html");
            const next = page.querySelector(".issue-workbench");
            if (!next || !next.querySelector(".issue-ledger")) throw new Error("List changed");
            if (controller.signal.aborted || location.href !== startUrl) return;
            if (document.activeElement !== startFocus && workbench.contains(document.activeElement)) return;
            const collapsed = new Set([...workbench.querySelectorAll(".issue-group:not([open])")].map(group => group.dataset.group));
            for (const group of next.querySelectorAll(".issue-group")) {
                if (collapsed.has(group.dataset.group)) group.open = false;
            }
            // Preserve pending search/filter edits and the open composer in the surrounding page.
            const controls = document.querySelector(".issue-controls");
            const updatedControls = page.querySelector(".issue-controls");
            if (controls && updatedControls) {
                controls.querySelector('input[name="inspect"]')?.remove();
                const selection = updatedControls.querySelector('input[name="inspect"]');
                if (selection) controls.append(selection);
                const layouts = updatedControls.querySelectorAll(".issue-view-tabs a");
                controls.querySelectorAll(".issue-view-tabs a").forEach((tab, n) => { tab.href = layouts[n].href; });
            }
            const focusWasInList = workbench.contains(document.activeElement) || document.activeElement === document.body;
            workbench.replaceWith(next);
            history.pushState({issuePreview: true}, "", url);
            renderedSearch = location.search;
            if (!focusWasInList) return;
            const title = next.querySelector("#preview-title");
            if (title) {
                title.focus({preventScroll: true});
                const top = next.querySelector("#issue-preview").getBoundingClientRect().top;
                if (top < 0 || top > window.innerHeight / 2) title.scrollIntoView({block: "center"});
            } else {
                focusRow(url.hash.slice("#record-".length));
            }
        } catch (error) {
            if (!controller.signal.aborted) location.assign(url);
        } finally {
            if (request === controller) {
                workbench.removeAttribute("aria-busy");
                request = undefined;
            }
        }
    }

    document.addEventListener("click", event => {
        if (event.button !== 0 || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.defaultPrevented) return;
        const link = event.target.closest?.(".issue-preview-trigger, .preview-close");
        if (!link) return;
        event.preventDefault();
        inspect(link);
    });

    document.addEventListener("keydown", event => {
        if (event.defaultPrevented || event.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
        const preview = event.target.closest?.(".issue-preview");
        if (preview && event.key === "Escape") {
            event.preventDefault();
            if (current().querySelector(".issue-register").getClientRects().length) {
                focusRow(preview.dataset.issue);
            } else {
                inspect(preview.querySelector(".preview-close"));
            }
            return;
        }
        const link = event.target.closest?.(".issue-ledger-row, .issue-preview-trigger");
        if (!link) return;
        const row = link.closest(".issue-record");
        if (event.key === " " && !event.repeat) {
            event.preventDefault();
            inspect(row.querySelector(".issue-preview-trigger"));
        } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            const links = [...current().querySelectorAll(".issue-ledger-row")].filter(a => a.getClientRects().length);
            const index = links.indexOf(row.querySelector(".issue-ledger-row"));
            event.preventDefault();
            links[index + (event.key === "ArrowDown" ? 1 : -1)]?.focus();
        }
    });

    // Back/forward may also traverse plain form submissions. Ask the URL for its authoritative view.
    window.addEventListener("popstate", () => {
        if (location.search !== renderedSearch) location.reload();
    });
})();
// The native disclosure and form also work without these focus conveniences.
(() => {
    "use strict";
    const composer = document.querySelector(".issue-compose");
    if (!composer) return;
    const trigger = composer.querySelector(":scope > summary");
    const close = composer.querySelector("[data-close-compose]");
    const title = composer.querySelector('input[name="title"]');
    close.hidden = false;
    function dismiss() {
        composer.open = false;
        trigger.focus({preventScroll: true});
    }
    composer.addEventListener("toggle", () => {
        if (composer.open && document.activeElement === trigger) title.focus({preventScroll: true});
        if (!composer.open && location.hash === "#new-issue")
            history.replaceState(history.state, "", location.pathname + location.search);
    });
    close.addEventListener("click", dismiss);
    composer.addEventListener("keydown", event => {
        if (composer.open && event.key === "Escape" && !event.defaultPrevented && !event.isComposing) {
            event.preventDefault();
            dismiss();
        }
    });
    composer.addEventListener("focusout", event => {
        if (composer.open && event.relatedTarget && !composer.contains(event.relatedTarget)) composer.open = false;
    });
    document.addEventListener("pointerdown", event => {
        if (composer.open && !composer.contains(event.target)) composer.open = false;
    });
})();
