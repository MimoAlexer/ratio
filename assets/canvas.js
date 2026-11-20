import {mountGestures} from "./canvas-gestures.js";
import {mountInspector, renderDocument} from "./canvas-render.js";
import {createPersistence} from "./canvas-persistence.js";
import {mountFeatures} from "./canvas-features.js";

const editor = document.querySelector("[data-canvas-editor]");
if (editor) {
    startEditor(editor).catch(() => {
        const message = editor.querySelector("#canvas-status");
        if (message) message.textContent = "The canvas could not open. Reload to try again.";
    });
}

async function startEditor(root) {
    const stage = root.querySelector("#canvas-stage");
    const world = root.querySelector("#canvas-world");
    const title = root.querySelector("#canvas-title");
    const heading = root.querySelector("#canvas-page-heading");
    const status = root.querySelector("#canvas-status");
    const csrf = root.dataset.csrf;
    let board, selected = null, trace = null, gestures, inspector;
    let locked = false;
    let snap = true;
    let pendingFrame = 0;
    let renameTimer;
    const undoStack = [], redoStack = [];
    const subscriptions = new Set();

    function updateTitle() {
        document.title = board.title + " · Ratio";
        if (heading) heading.textContent = board.title;
    }
    // Snap is a drawing preference on this device and carries between its boards.
    try { snap = localStorage.getItem("ratio.canvas.snap") !== "off"; } catch { /* Storage is optional. */ }
    function notify(text) { if (status.textContent !== text) status.textContent = text; }
    function emit() {
        for (const fn of subscriptions) fn();
    }
    function viewChanged() {
        if (pendingFrame) return;
        pendingFrame = requestAnimationFrame(() => { pendingFrame = 0; emit(); });
    }
    function canEdit() { return Boolean(board && !board.read_only && !locked); }
    function record(id) {
        if (!board || !id) return null;
        return board.document.nodes.find(n => n.id === id) ||
            board.document.edges.find(e => e.id === id) ||
            board.document.strokes.find(s => s.id === id) || null;
    }
    function paint() {
        if (!board) return;
        renderDocument(world, board.document, selected, trace);
        root.dataset.empty = String(!board.document.nodes.length && !board.document.strokes.length);
    }
    function updateControls() {
        if (!board) return;
        root.dataset.readOnly = String(board.read_only);
        title.readOnly = !canEdit();
        for (const button of root.querySelectorAll("button[data-tool]")) {
            button.disabled = !canEdit() && !["select","pan"].includes(button.dataset.tool);
        }
        for (const button of root.querySelectorAll("button[data-command]")) {
            switch (button.dataset.command) {
                case "undo": button.disabled = !canEdit() || undoStack.length === 0; break;
                case "redo": button.disabled = !canEdit() || redoStack.length === 0; break;
                case "delete": button.disabled = !canEdit() || !selected; break;
                case "snap": button.setAttribute("aria-pressed", String(snap)); break;
            }
        }
    }
    function redraw() {
        paint(); updateControls(); inspector?.update(); emit();
    }
    function select(id) {
        const next = record(id) ? id : null;
        if (selected === next) return;
        if (inspector?.commitPending?.() === false) return;
        selected = next;
        redraw();
    }
    function setTrace(next) {
        trace = next; paint();
    }
    function snapshot() { return {document:board.document,title:board.title,selection:selected}; }
    function pushHistory(stack, entry) {
        // Document references are immutable; a geometry drag contributes one entry.
        stack.push(entry);
        if (stack.length > 40) stack.shift();
    }
    function commit(nextDocument, nextTitle) {
        pushHistory(undoStack, snapshot());
        redoStack.length = 0;
        board = {...board, document:nextDocument, title:nextTitle};
        if (selected && !record(selected)) selected = null;
        persistence.dirty();
        redraw();
    }
    function change(mutate) {
        if (!canEdit()) return false;
        const next = typeof structuredClone === "function" ? structuredClone(board.document) :
            JSON.parse(JSON.stringify(board.document));
        mutate(next);
        if (next.nodes.length > 250 || next.edges.length > 500 || next.strokes.length > 200 ||
            next.strokes.some(s => s.points.length > 2048) ||
            next.strokes.reduce((sum,s) => sum + s.points.length,0) > 20000) {
            notify("This board has reached its drawing limit."); return false;
        }
        if (JSON.stringify(next) === JSON.stringify(board.document)) return false;
        commit(next, board.title);
        return true;
    }
    function commitTitle(normalize = true) {
        clearTimeout(renameTimer);
        if (!board || !canEdit()) return true;
        const value = title.value.trim();
        if (!value) {
            if (!normalize) return false;
            title.value = board.title;
            notify("A board needs a name. The previous name was kept.");
            return true;
        }
        if (value !== board.title) {
            commit(board.document, value);
            updateTitle();
        }
        if (normalize) title.value = value;
        return true;
    }
    function commitFields() {
        return commitTitle() && inspector?.commitPending?.() !== false;
    }
    function restoreHistory(from, to) {
        if (!canEdit() || !commitFields() || !from.length) return;
        const previous = from.pop();
        pushHistory(to,snapshot());
        inspector?.discardPending?.();
        board = {...board,title:previous.title,document:previous.document};
        selected = record(previous.selection) ? previous.selection : null;
        trace = null;
        title.value = board.title;
        updateTitle();
        persistence.dirty();
        redraw();
    }
    function removeSelection() {
        const id = selected;
        if (!id || !canEdit() || !commitFields()) return;
        change(doc => {
            doc.nodes = doc.nodes.filter(n => n.id !== id);
            doc.edges = doc.edges.filter(e => e.id !== id && e.from !== id && e.to !== id);
            doc.strokes = doc.strokes.filter(s => s.id !== id);
        });
        stage.focus({preventScroll:true});
    }
    function adopt(next) {
        const initial = !board;
        clearTimeout(renameTimer);
        inspector?.discardPending?.();
        board = next;
        selected = record(selected) ? selected : null;
        trace = null;
        undoStack.length = 0; redoStack.length = 0;
        title.value = board.title;
        updateTitle();
        if (!initial) redraw();
    }
    function dropMissingSources(items) {
        // A linked record can disappear between editing and saving the board.
        // Undo must not resurrect an attachment which the server already removed.
        const missing = new Map(items.map(item => [item.node,item]));
        function detach(doc) {
            let modified = false;
            const nodes = doc.nodes.map(node => {
                const entry = missing.get(node.id);
                if (!entry || node.source?.kind !== entry.kind || node.source.id !== entry.id) return node;
                const clean = {...node};
                delete clean.source;
                modified = true;
                return clean;
            });
            return modified ? {...doc,nodes} : doc;
        }
        for (const entry of [...undoStack,...redoStack]) entry.document = detach(entry.document);
        board = {...board,document:detach(board.document)};
        redraw();
    }
    const persistence = createPersistence({
        id:root.dataset.id, csrf, root, getBoard:()=>board, adopt, dropMissingSources,
        updateRevision:revision => { if (board) board = {...board,revision}; },
        notify, isBusy:()=>locked || gestures?.isBusy?.() || Boolean(
            root.contains(document.activeElement) && document.activeElement.closest("input,textarea,select,[contenteditable=true]") ||
            document.querySelector("dialog[open]"))
    });
    if (!await persistence.load()) return;
    gestures = mountGestures({
        root,stage,world,getBoard:()=>board,getSelection:()=>selected,select,change,notify,canEdit,
        isSnapped:()=>snap,render:paint,changedView:viewChanged,editSelection:()=>inspector?.editSelection()
    });
    inspector = mountInspector({
        host:root.querySelector("#canvas-inspector"), root, getBoard:()=>board,
        getSelection:()=>selected, select, change, notify, removeSelection, canEdit,
        flush:async()=>commitFields() && await persistence.flush()
    });
    const api = {
        root,csrf,getBoard:()=>board,getSelection:()=>selected,select,change,notify,setTrace,
        getView:()=>gestures.getView(),centerOn:(x,y)=>gestures.centerOn(x,y),fit:()=>gestures.fit(),
        onChange(fn) { subscriptions.add(fn); return ()=>subscriptions.delete(fn); },
        flush:async()=>commitFields() && await persistence.flush(),
        reloadBoard:next=>persistence.reload(next),
        async withBoardLock(worker) {
            if (locked || !canEdit() || !commitFields()) throw new Error("Finish editing before using checkpoints.");
            locked = true; updateControls(); inspector.update();
            try {
                if (!await persistence.flush()) throw new Error("Save your changes before using checkpoints.");
                return await worker();
            } finally {
                locked = false; updateControls(); inspector.update();
            }
        }
    };
    function chooseTool(tool) {
        if (!canEdit() && !["pan","select"].includes(tool)) return;
        gestures.setTool(tool); root.dataset.tool = tool;
        stage.focus({preventScroll:true});
    }
    function command(name) {
        switch(name) {
            case "undo": restoreHistory(undoStack,redoStack); break;
            case "redo": restoreHistory(redoStack,undoStack); break;
            case "fit": gestures.fit(); break;
            case "zoom-in": gestures.zoomBy(1.25); break;
            case "zoom-out": gestures.zoomBy(0.8); break;
            case "delete": removeSelection(); break;
            case "snap":
                snap = !snap;
                try { localStorage.setItem("ratio.canvas.snap", snap ? "on" : "off"); } catch { /* Optional. */ }
                updateControls();
                notify(snap ? "Snap to grid on" : "Snap to grid off");
                break;
            default: return false;
        }
        return true;
    }
    root.addEventListener("click", event => {
        const tool = event.target.closest("button[data-tool]");
        if (tool) {
            chooseTool(tool.dataset.tool);
            const group = tool.closest("details");
            if (group) group.open = false;
            return;
        }
        const button = event.target.closest("button[data-command]");
        if (button) command(button.dataset.command);
    });
    title.addEventListener("input", () => {
        clearTimeout(renameTimer);
        renameTimer = setTimeout(() => commitTitle(false), 450);
    });
    title.addEventListener("change",()=>commitTitle());
    window.addEventListener("beforeunload", event => {
        if (canEdit() && title.value.trim() && title.value.trim() !== board.title) {
            commitTitle(false);
            event.preventDefault();
            event.returnValue = "";
        }
    });
    title.addEventListener("keydown",event=>{
        if (event.key === "Enter") { event.preventDefault(); commitTitle(); title.blur(); }
        if (event.key === "Escape") { title.value = board.title; title.blur(); }
    });
    function cycle(direction) {
        const doc = board.document;
        const items = [...doc.nodes,...doc.edges,...doc.strokes];
        if (!items.length) return;
        const current = items.findIndex(item => item.id === selected);
        const index = current < 0 ? (direction > 0 ? 0 : items.length - 1) :
            (current + direction + items.length) % items.length;
        const item = items[index];
        select(item.id);
        if ("x" in item) gestures.centerOn(item.x+item.w/2,item.y+item.h/2);
        notify(item.label || (item.from ? "Connector" : "Drawing"));
    }
    const directions = {ArrowLeft:[-1,0], ArrowRight:[1,0], ArrowUp:[0,-1], ArrowDown:[0,1]};
    const keys = {v:"select",h:"pan",n:"note",p:"pen",c:"connect"};
    root.addEventListener("keydown",event=>{
        if (event.defaultPrevented || event.isComposing || event.altKey ||
            event.target.closest("input,textarea,select,[contenteditable=true],dialog,summary,button,a") ||
            document.querySelector("dialog[open]")) return;
        const key = event.key.toLowerCase();
        const modified = event.ctrlKey || event.metaKey;
        if (modified && (key === "z" || key === "y")) {
            event.preventDefault();
            command(key === "y" || event.shiftKey ? "redo" : "undo");
            return;
        }
        if (modified || !stage.contains(event.target)) return;
        if (event.key === "Escape") {
            gestures.cancel(); chooseTool("select"); select(null);
        } else if (event.key === "Delete" || event.key === "Backspace") {
            removeSelection();
        } else if (event.key === "]" || event.key === "[") {
            cycle(event.key === "]" ? 1 : -1);
        } else if (event.key in directions) {
            const [dx,dy] = directions[event.key];
            const item = record(selected);
            if (canEdit() && item && "x" in item && gestures.getTool() === "select") {
                const step = event.shiftKey ? 32 : snap ? 16 : 1;
                change(doc=>{
                    const node = doc.nodes.find(n=>n.id===selected);
                    node.x = Math.max(-16000,Math.min(16000,node.x+dx*step));
                    node.y = Math.max(-16000,Math.min(16000,node.y+dy*step));
                });
            } else if (gestures.getTool() === "pan") {
                const view = gestures.getView();
                gestures.centerOn(view.x+view.width*(.5+dx*.12),view.y+view.height*(.5+dy*.12));
            } else cycle(dx+dy);
        } else if (key in keys) chooseTool(keys[key]);
        else if (event.key === "+" || event.key === "=") gestures.zoomBy(1.25);
        else if (event.key === "-") gestures.zoomBy(.8);
        else if (event.key === "0") gestures.fit();
        else if (event.key === "?") document.querySelector("[data-shortcuts-help]:not([hidden])")?.click();
        else if (event.key === "Enter" && selected && gestures.getTool()==="select") inspector.editSelection();
        else return;
        event.preventDefault();
    });
    root.dataset.ready = "true";
    gestures.setTool("select");
    redraw();
    mountFeatures(api);
    gestures.initialView();
}
