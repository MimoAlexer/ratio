// Board revisions serialize writes on the server. Each editor tab also keeps its
// own recovery copy so concurrent tabs cannot delete one another's changes.
export function createPersistence({id, csrf, root, getBoard, adopt, updateRevision, notify, isBusy = () => false, dropMissingSources}) {
    const url = '/api/canvas/' + encodeURIComponent(id);
    const owner = root.dataset.owner;
    const prefix = owner ? 'ratio.canvas.v1.' + owner + '.' + id + '.' : null;
    const writer = crypto.randomUUID();
    // The note editor rotates this owner generation on sign-out and deletion.
    // Reusing it also invalidates sessionStorage in tabs closed during cleanup.
    const epochKey = owner && 'workspace:note-draft-epoch-v1:' + owner;
    function accountEpoch(initialize = false) {
        if (!epochKey) return null;
        try {
            if (initialize && localStorage.getItem(epochKey) === null) localStorage.setItem(epochKey, 'initial-' + crypto.randomUUID());
            return localStorage.getItem(epochKey);
        } catch { return null; }
    }
    const epoch = accountEpoch(true);
    const retention = 30 * 86400000;
    const native = /(?:^|\s)(?:RatioDesktop|WorkingSetDesktop)\//.test(navigator.userAgent);
    const draftAction = native ? 'Copy draft' : 'Download draft';
    const tabKey = prefix && prefix + 'tab';
    const copyKey = prefix && prefix + 'copy.' + writer;
    let actions = root.querySelector('#canvas-save-actions');
    if (!actions) {
        actions = document.createElement('span');
        actions.id = 'canvas-save-actions';
        root.querySelector('#canvas-status')?.after(actions);
    }
    let loaded = false;
    let unsaved = false;
    let sequence = 0;
    let saving = null;
    let reading = false;
    let timer;
    let stopped = false;
    let conflict = null;
    let inherited = null;
    let durable = false;
    let retrying = false;
    let revoked = false;

    function status(text, choices = []) {
        notify(text);
        actions.replaceChildren();
        actions.hidden = choices.length === 0;
        for (const [label, action] of choices) {
            const button = document.createElement('button');
            button.type = 'button';
            button.className = 'action-quiet';
            button.textContent = label;
            button.addEventListener('click', action);
            actions.append(button);
        }
    }

    function validDocument(doc) {
        return doc?.version === 1 && Array.isArray(doc.nodes) &&
            Array.isArray(doc.edges) && Array.isArray(doc.strokes);
    }

    function validBoard(board) {
        return board?.id === id && Number.isSafeInteger(board.revision) &&
            typeof board.title === 'string' && typeof board.read_only === 'boolean' && validDocument(board.document);
    }

    function validDraft(draft) {
        return draft?.id === id && typeof draft.writer === 'string' &&
            Number.isSafeInteger(draft.revision) && typeof draft.title === 'string' && validDocument(draft.document);
    }

    function sameValue(a, b) {
        if (a === b) return true;
        if (!a || !b || typeof a !== 'object' || typeof b !== 'object') return false;
        if (Array.isArray(a) !== Array.isArray(b)) return false;
        const keys = Object.keys(a);
        return keys.length === Object.keys(b).length && keys.every(key =>
            Object.hasOwn(b, key) && sameValue(a[key], b[key])
        );
    }

    function sameContent(a, b) {
        return a.title === b.title && sameValue(a.document, b.document);
    }

    function readCopy(storage, key) {
        try {
            const value = JSON.parse(storage.getItem(key));
            if (!value) return null;
            if (!epoch || value.epoch !== epoch || !Number.isFinite(value.time) || value.time < Date.now() - retention) {
                storage.removeItem(key);
                return null;
            }
            return validDraft(value) ? value : null;
        } catch { return null; }
    }

    function currentCopy() {
        if (!tabKey) return null;
        try { return readCopy(sessionStorage, tabKey); } catch { return null; }
    }

    function otherCopy() {
        if (!prefix) return null;
        try {
            let latest = null;
            for (const key of Object.keys(localStorage)) {
                if (!key.startsWith('ratio.canvas.v1.' + owner + '.')) continue;
                // Visiting another board also releases expired owner-scoped
                // recovery files. Snap preferences and other owners stay intact.
                const item = readCopy(localStorage, key);
                if (!key.startsWith(prefix + 'copy.')) continue;
                if (item && (!latest || item.time > latest.time)) latest = item;
            }
            return latest;
        } catch { return null; }
    }

    function remember() {
        if (!copyKey || !epoch || revoked || accountEpoch() !== epoch) return false;
        const {title, revision, document: diagram} = getBoard();
        const data = JSON.stringify({id, writer, epoch, time: Date.now(), title, revision, document: diagram});
        let stored = false;
        try { localStorage.setItem(copyKey, data); stored = true; } catch { /* Storage can be full. */ }
        try { sessionStorage.setItem(tabKey, data); stored = true; } catch { /* Private browsing. */ }
        return stored;
    }

    function retire(draft) {
        if (!prefix || !draft) return;
        const key = prefix + 'copy.' + draft.writer;
        try {
            if (readCopy(localStorage, key)?.time === draft.time) localStorage.removeItem(key);
        } catch { /* Recovery is best effort. */ }
    }

    function forget() {
        if (!prefix) return;
        try { localStorage.removeItem(copyKey); } catch { /* Optional cache. */ }
        try {
            const current = currentCopy();
            if (current?.writer === writer || (current?.writer === inherited?.writer && current?.time === inherited?.time)) {
                sessionStorage.removeItem(tabKey);
            }
        } catch { /* Optional cache. */ }
        retire(inherited);
        inherited = null;
        durable = false;
    }

    async function request(options = {}, suffix = '') {
        const controller = new AbortController();
        const timeout = setTimeout(() => controller.abort(), 16000);
        try {
            const response = await fetch(url + suffix, {
                credentials: 'same-origin', mode: 'same-origin', redirect: 'error', cache: 'no-store',
                ...options, headers: {Accept: 'application/json', ...options.headers}, signal: controller.signal,
            });
            if (response.status === 204) return null;
            if (!response.ok) {
                const error = new Error('Canvas request failed');
                error.status = response.status;
                if (response.status === 409) {
                    error.quota = (await response.text()).toLowerCase().includes('workspace storage limit');
                }
                throw error;
            }
            return await response.json();
        } finally { clearTimeout(timeout); }
    }

    async function fetchBoard(suffix = '') {
        const value = await request({}, suffix);
        if (value === null && suffix) return null;
        if (!validBoard(value)) throw new Error('Invalid board response');
        return value;
    }

    // The exported object is the document the user can see, including local
    // changes that never reached the server. It contains no session credentials.
    function downloadDraft() {
        const {title, document: diagram} = getBoard();
        const content = JSON.stringify({title, document: diagram}, null, 2);
        if (native) {
            const dialog = document.createElement('dialog');
            dialog.className = 'canvas-draft-dialog';
            dialog.setAttribute('aria-label', 'Copy canvas draft');
            const heading = document.createElement('h2');
            heading.textContent = 'Copy canvas draft';
            const text = document.createElement('textarea');
            text.readOnly = true;
            text.setAttribute('aria-label', 'Canvas draft JSON');
            text.value = content;
            const copy = document.createElement('button');
            copy.type = 'button';
            copy.textContent = 'Copy';
            copy.addEventListener('click', async () => {
                try { await navigator.clipboard.writeText(content); copy.textContent = 'Copied'; }
                catch { text.focus(); text.select(); copy.textContent = 'Use Ctrl+C to copy'; }
            });
            const close = document.createElement('button');
            close.type = 'button';
            close.textContent = 'Close';
            close.addEventListener('click', () => dialog.close());
            dialog.addEventListener('close', () => dialog.remove(), {once:true});
            dialog.append(heading, text, copy, close);
            root.append(dialog);
            dialog.showModal();
            text.focus();
            text.select();
            return;
        }
        const data = new Blob([content], {type: 'application/json'});
        const href = URL.createObjectURL(data);
        const link = document.createElement('a');
        link.href = href;
        link.download = (title.replace(/[^\p{L}\p{N} ._-]+/gu, '').trim().slice(0, 90) || 'Canvas') + '.draft.json';
        link.click();
        setTimeout(() => URL.revokeObjectURL(href), 10000);
    }

    function showConflict() {
        const choices = [[draftAction, downloadDraft], ['Use server version', useServer]];
        if (!conflict.read_only) choices.push(['Replace server version', replaceServer]);
        const state = conflict.read_only ? 'This project is read-only.' : 'Another version is on the server.';
        status(state + (durable ? ' Your edits are saved on this device.' : ' Your edits are open here.'), choices);
    }

    function showFailure(error) {
        stopped = true;
        let message = 'Couldn’t save. Reconnect and retry.';
        if (error.status === 401) message = 'Your session expired. Sign in, then retry.';
        if (error.status === 403) message = 'Saving is restricted for this board.';
        if (error.status === 404) message = 'This board is no longer available.';
        if ([400, 413, 422].includes(error.status)) message = 'The server could not accept this diagram. Check its contents and size.';
        if (error.status === 409) message = error.quota ? 'Workspace storage is full. Free space, then retry.' : 'The server could not save this change.';
        status(message, [['Retry', retry], [draftAction, downloadDraft]]);
    }

    async function useServer() {
        if (!window.confirm('Discard the local edits in this tab and open the version on the server?')) return;
        const observed = sequence;
        try {
            const latest = await fetchBoard();
            if (revoked) return;
            if (sequence !== observed || saving) { notify('Changes are still being edited. Try again.'); return; }
            unsaved = false;
            stopped = false;
            conflict = null;
            adopt(latest);
            forget();
            status(latest.read_only ? 'Read-only' : 'Saved');
        } catch (error) { showFailure(error); }
    }

    function replaceServer() {
        if (!conflict || conflict.read_only || saving) return;
        if (!window.confirm('Replace server revision ' + conflict.revision + ' with the current local diagram?')) return;
        updateRevision(conflict.revision);
        conflict = null;
        stopped = false;
        durable = remember();
        void flush();
    }

    function recover(server, draft) {
        inherited = draft;
        if (sameContent(server, draft)) {
            forget();
            status(server.read_only ? 'Read-only' : 'Saved');
            return;
        }
        adopt({...server, document: draft.document, title: draft.title, revision: draft.revision});
        unsaved = true;
        sequence++;
        durable = remember();
        if (server.read_only || draft.revision !== server.revision) {
            conflict = server;
            stopped = true;
            showConflict();
        } else {
            status('Recovered edits. Saving…');
            void flush();
        }
    }

    async function load() {
        try {
            const server = await fetchBoard();
            if (revoked) return false;
            adopt(server);
            loaded = true;
            status(server.read_only ? 'Read-only' : 'Saved');
            const current = currentCopy();
            if (current) recover(server, current);
            else {
                const other = otherCopy();
                if (other && sameContent(other, server)) retire(other);
                else if (other) {
                    status('A draft is available from another session.', [['Open draft', () => recover(getBoard(), other)]]);
                }
            }
            return true;
        } catch {
            status('Couldn’t open this board.', [['Retry', () => window.location.reload()]]);
            return false;
        }
    }

    function dirty() {
        if (!loaded || revoked || getBoard().read_only) return;
        unsaved = true;
        sequence++;
        durable = remember();
        if (conflict) { showConflict(); return; }
        stopped = false;
        status(durable ? 'Saving…' : 'Saving… Keep this tab open.');
        clearTimeout(timer);
        timer = setTimeout(flush, 600);
    }

    async function saveLoop() {
        while (unsaved && !stopped) {
            const observed = sequence;
            const {title, revision, document: diagram} = getBoard();
            const body = JSON.stringify({csrf, revision, title, document: diagram});
            let acceptedRevision;
            try {
                const result = await request({method: 'POST', headers: {'Content-Type': 'application/json'}, body});
                if (!Number.isSafeInteger(result?.revision) || result.revision < revision) throw new Error('Invalid save response');
                acceptedRevision = result.revision;
                if (revoked) return false;
                if (Array.isArray(result.detached) && result.detached.length) dropMissingSources?.(result.detached);
            } catch (error) {
                if (revoked) return false;
                if (error.status === 409) {
                    try {
                        const latest = await fetchBoard();
                        if (revoked) return false;
                        if (!latest.read_only && latest.revision !== revision && sameContent(latest, JSON.parse(body))) {
                            // A previous request can succeed while its response is lost.
                            acceptedRevision = latest.revision;
                        } else if (latest.revision !== revision || latest.read_only) {
                            conflict = latest;
                            stopped = true;
                            showConflict();
                            return false;
                        }
                    } catch (readError) { showFailure(readError); return false; }
                }
                if (acceptedRevision === undefined) { showFailure(error); return false; }
            }
            updateRevision(acceptedRevision);
            if (observed === sequence) {
                unsaved = false;
                stopped = false;
                conflict = null;
                forget();
                status('Saved');
            } else {
                durable = remember();
            }
        }
        return !unsaved;
    }

    function flush() {
        clearTimeout(timer);
        if (saving) return saving;
        if (!loaded || revoked || stopped || (unsaved && getBoard().read_only)) return Promise.resolve(false);
        if (!unsaved) return Promise.resolve(true);
        saving = saveLoop().finally(() => { saving = null; });
        return saving;
    }

    async function retry() {
        if (retrying) return;
        if (conflict) { showConflict(); return; }
        retrying = true;
        stopped = false;
        try { await flush(); } finally { retrying = false; }
    }

    function reload(server) {
        if (!validBoard(server) || !loaded || revoked) return;
        if (server.revision < getBoard().revision) return;
        if (unsaved || saving) {
            conflict = server;
            stopped = true;
            showConflict();
            return;
        }
        conflict = null;
        stopped = false;
        adopt(server);
        status(server.read_only ? 'Read-only' : 'Saved');
    }

    async function refresh() {
        if (!loaded || revoked || reading || unsaved || saving || document.hidden || isBusy()) return;
        const {revision, read_only} = getBoard();
        const observed = sequence;
        reading = true;
        try {
            const latest = await fetchBoard('?since=' + encodeURIComponent(revision) + '&read_only=' + read_only);
            if (latest && !unsaved && !saving && !isBusy() && observed === sequence &&
                (latest.revision !== getBoard().revision || latest.read_only !== getBoard().read_only)) reload(latest);
        } catch { /* Recheck an unavailable connection when the tab regains focus. */ }
        finally { reading = false; }
    }

    function signedOut() {
        if (revoked) return;
        revoked = true;
        stopped = true;
        sequence++;
        unsaved = false;
        clearTimeout(timer);
        forget();
        // A tab can have recovery entries from other boards visited before
        // this one. Its sessionStorage cannot be cleared by the sign-out tab.
        try {
            for (const key of Object.keys(sessionStorage)) {
                if (key.startsWith('ratio.canvas.v1.' + owner + '.')) sessionStorage.removeItem(key);
            }
        } catch { /* The rotated generation still invalidates old entries. */ }
        if (loaded) adopt({...getBoard(), read_only: true});
        window.location.replace('/login');
    }
    window.addEventListener('storage', event => {
        if (epochKey && (event.key === epochKey || event.key === null) && accountEpoch() !== epoch) signedOut();
    });
    try {
        const channel = new BroadcastChannel('workspace-note-drafts-v1');
        channel.addEventListener('message', event => {
            const message = event.data;
            if (message?.type === 'logout' && message.user === owner && message.epoch !== epoch) signedOut();
        });
    } catch { /* The durable generation also propagates through storage events. */ }
    window.addEventListener('beforeunload', event => {
        if (unsaved) { event.preventDefault(); event.returnValue = ''; }
    });
    window.addEventListener('online', () => { if (unsaved && !conflict) void retry(); else void refresh(); });
    window.addEventListener('focus', refresh);
    document.addEventListener('visibilitychange', () => {
        if (!document.hidden) void refresh();
        else if (unsaved && !stopped) void flush();
    });
    setInterval(refresh, 15000);
    return {load, dirty, flush, reload, isDirty: () => unsaved};
}
