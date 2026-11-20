// Each page carries the workspace revision read before its content was queried.
(() => {
    "use strict";
    const rendered = document.querySelector('meta[name="workspace-revision"]')?.content;
    const content = document.querySelector(".page-content[data-view]");
    const desktop = /(?:^|\s)(?:RatioDesktop|WorkingSetDesktop)\//.test(navigator.userAgent);
    // The canvas editor synchronizes its own board revision without reloading an active drawing.
    if (!rendered || !content || !desktop || content.querySelector("[data-canvas-editor]")) return;

    const readViews = new Set(["home", "issues", "inbox", "notes", "projects", "canvas", "roadmap", "releases", "insights", "health"]);
    const fields = 'input:not([type="hidden"], [type="button"], [type="submit"], [type="reset"], [type="image"]), textarea, select, [contenteditable]:not([contenteditable="false"]), [role="textbox"], [role="combobox"]';
    const notice = document.createElement("div");
    notice.className = "notice row";
    notice.hidden = true;
    const message = document.createElement("span");
    message.setAttribute("role", "status");
    const load = document.createElement("button");
    load.type = "button";
    load.className = "secondary";
    load.textContent = "Load updates";
    const signIn = document.createElement("a");
    signIn.href = "/desktop/start";
    signIn.textContent = "Sign in";
    notice.append(message, load, signIn);
    content.prepend(notice);

    let interacted = false;
    let offered = false;
    let checking = false;
    let reloading = false;
    for (const event of ["input", "change", "compositionstart"]) {
        document.addEventListener(event, e => {
            if (e.target instanceof Element && !e.target.closest(".command-dialog") && e.target.closest(fields)) interacted = true;
        }, true);
    }
    window.addEventListener("submit", event => {
        interacted = true;
        const form = event.target;
        if (navigator.onLine || event.defaultPrevented || !(form instanceof HTMLFormElement)) return;
        const method = event.submitter?.getAttribute("formmethod") ?? form.getAttribute("method");
        if (method?.toLowerCase() !== "post") return;
        const action = event.submitter?.getAttribute("formaction") ?? form.getAttribute("action");
        let destination;
        try { destination = new URL(action || location.href, document.baseURI); } catch { return; }
        if (destination.origin !== location.origin) return;

        event.preventDefault();
        event.stopPropagation();
        let feedback = form.querySelector("[data-offline-submission]");
        if (!feedback) {
            feedback = document.createElement("p");
            feedback.className = "notice";
            feedback.dataset.offlineSubmission = "";
            feedback.setAttribute("role", "status");
            form.append(feedback);
        }
        feedback.textContent = "Your submission wasn’t sent. Reconnect and try again.";
    }, true);
    function show(text, action) {
        if (message.textContent !== text) message.textContent = text;
        load.hidden = action !== load;
        signIn.hidden = action !== signIn;
        notice.hidden = !text;
    }

    // Browser restoration, autofill and the note draft restorer can change a field
    // without dispatching input. Check current controls before replacing the DOM.
    function hasDraft() {
        if (interacted) return true;
        for (const field of document.querySelectorAll("input, textarea, select")) {
            if (!field.matches(fields)) continue;
            if (field instanceof HTMLSelectElement) {
                const options = Array.from(field.options);
                const defaults = options.filter(option => option.defaultSelected);
                const selected = !field.multiple && field.size <= 1 && !defaults.length
                    ? [options.find(option => !option.disabled && !option.closest("optgroup[disabled]"))]
                    : field.multiple ? defaults : defaults.slice(-1);
                if (options.some(option => option.selected !== selected.includes(option))) return true;
            } else if (field.type === "radio" || field.type === "checkbox") {
                if (field.checked !== field.defaultChecked) return true;
            } else if (field.value !== field.defaultValue) {
                return true;
            }
        }
        return false;
    }
    function visibleEditor(field) {
        if (field.disabled || field.readOnly || field.closest("[hidden], details:not([open])")) return false;
        const bounds = field.getBoundingClientRect();
        return bounds.width > 0 && bounds.height > 0 &&
            bounds.bottom > 0 && bounds.top < document.documentElement.clientHeight &&
            bounds.right > 0 && bounds.left < document.documentElement.clientWidth;
    }
    function holdsInteraction() {
        if (!readViews.has(content.dataset.view) || hasDraft() || document.activeElement?.closest(fields) ||
            document.querySelector('dialog[open], [role="dialog"]:not([hidden]), .mobile-menu[open]') ||
            window.getSelection()?.isCollapsed === false) return true;
        // An expanded editor remains in place even before the first keystroke.
        return Array.from(document.querySelectorAll(fields)).some(field =>
            visibleEditor(field) && (field.closest('form[method="post" i], details[open]') || field.isContentEditable));
    }
    function allowLeaving() {
        return !hasDraft() || window.confirm("Leave this page and discard unsaved edits?");
    }
    load.addEventListener("click", () => {
        if (allowLeaving()) location.reload();
    });
    signIn.addEventListener("click", event => {
        if (!allowLeaving()) event.preventDefault();
    });

    function offline() {
        show("You’re offline. Reconnect to load or save changes.");
    }
    async function check() {
        if (document.hidden || checking || reloading) return;
        if (!navigator.onLine) { offline(); return; }
        checking = true;
        const controller = new AbortController();
        const timeout = setTimeout(() => controller.abort(), 8000);
        try {
            const response = await fetch("/api/device/changes", {
                headers: {Accept: "application/json"}, credentials: "same-origin",
                mode: "same-origin", cache: "no-store", redirect: "error", signal: controller.signal,
            });
            if (response.status === 401) {
                show("Your session expired. Sign in to load or save changes.", signIn);
                return;
            }
            if (!response.ok || !response.headers.get("content-type")?.includes("application/json")) throw new Error();
            const {revision} = await response.json();
            if (typeof revision !== "string" || !revision) throw new Error();
            if (document.hidden) return;
            if (!navigator.onLine) { offline(); return; }
            if (revision === rendered) {
                show("");
            } else if (offered || holdsInteraction()) {
                offered = true;
                show("Workspace updates are available.", load);
            } else {
                reloading = true;
                location.reload();
            }
        } catch {
            if (navigator.onLine) show("Can’t check for workspace updates. Retrying automatically.");
            else offline();
        } finally {
            clearTimeout(timeout);
            checking = false;
        }
    }
    document.addEventListener("visibilitychange", check);
    window.addEventListener("pageshow", event => {
        if (event.persisted) { reloading = false; check(); }
    });
    window.addEventListener("online", check);
    window.addEventListener("offline", offline);
    setInterval(check, 10000);
    check();
})();
