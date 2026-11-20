const svgNamespace = "http://www.w3.org/2000/svg";
const fontFamily = "'Instrument Sans', system-ui, sans-serif";
const kindNames = {process: "Process", service: "Service", database: "Database", note: "Note", decision: "Decision"};
const measureContext = document.createElement("canvas").getContext("2d");
const layersFor = new WeakMap();

function graphic(tag, attributes = {}, text) {
    const el = document.createElementNS(svgNamespace, tag);
    for (const [key, value] of Object.entries(attributes)) el.setAttribute(key, String(value));
    if (text !== undefined) el.textContent = text;
    return el;
}

function narrowText(value, width, size, weight) {
    const text = value.replace(/\s+/gu, " ").trim();
    if (!text || width < 7) return "";
    if (measureContext) measureContext.font = weight + " " + size + "px " + fontFamily;
    const measure = str => measureContext ? measureContext.measureText(str).width : Array.from(str).length * size * .58;
    if (measure(text) <= width) return text;
    const chars = Array.from(text);
    let low = 0, high = chars.length;
    while (low < high) {
        const mid = Math.ceil((low + high) / 2);
        if (measure(chars.slice(0, mid).join("") + "…") <= width) low = mid;
        else high = mid - 1;
    }
    return chars.slice(0, low).join("").trimEnd() + "…";
}

function groupFor(item, type, selection, trace, label) {
    const group = graphic("g", {
        class: "canvas-" + type, "data-item": item.id, "data-type": type,
        "data-selected": item.id === selection, role: "button", "aria-label": label,
        "aria-pressed": item.id === selection, tabindex: "-1",
    });
    if (trace && type !== "stroke") {
        const traced = (type === "node" ? trace.nodes : trace.edges).has(item.id);
        group.setAttribute("data-traced", traced);
        if (!traced) group.setAttribute("opacity", ".36");
    }
    return group;
}

function nodeShape(node) {
    const {w, h, kind} = node;
    if (kind === "decision") {
        return graphic("path", {d: `M${w / 2},0 L${w},${h / 2} L${w / 2},${h} L0,${h / 2}Z`});
    }
    if (kind === "note") {
        const fold = Math.min(14, w / 6, h / 5);
        return graphic("path", {d: `M4,0 H${w - fold} L${w},${fold} V${h - 4} Q${w},${h} ${w - 4},${h} H4 Q0,${h} 0,${h - 4} V4 Q0,0 4,0Z`});
    }
    if (kind === "database") {
        const cap = Math.min(10, h * .16);
        return graphic("path", {d: `M0,${cap} A${w / 2},${cap} 0 0 1 ${w},${cap} V${h - cap} A${w / 2},${cap} 0 0 1 0,${h - cap}Z`});
    }
    return graphic("rect", {x: 0, y: 0, width: w, height: h, rx: 8});
}

function nodeDetails(group, node) {
    const {kind, h, w} = node;
    const ink = "#737373";
    if (kind === "note") {
        const fold = Math.min(14, w / 6, h / 5);
        group.append(graphic("path", {d: `M${w-fold},1 V${fold} H${w-1}`, fill: "none", stroke: ink}));
    }
    if (kind === "database") {
        const cap = Math.min(10, h * .16);
        group.append(graphic("path", {d: `M1,${cap} A${w / 2 - 1},${cap} 0 0 0 ${w-1},${cap}`, fill: "none", stroke: ink}));
    }
    if (node.source) {
        const pinX = kind === "decision" ? w * .5 : w - 16;
        const pinY = kind === "database" ? h - 15 : kind === "decision" ? h - 18 : h - 11;
        group.append(graphic("path", {
            class: "canvas-source-mark", d: "M1 8l2-2m0 0 3-3m-3 3-1-1a2 2 0 0 1 0-3l1-1a2 2 0 0 1 3 0m0 2 1 1a2 2 0 0 1 0 3L6 9a2 2 0 0 1-3 0",
            transform: `translate(${pinX - 3} ${pinY - 4})`, fill: "none",
            stroke: "#b4b4b4", "stroke-linecap": "round", "stroke-width": 1.2,
        }));
    }
}

function nodeContents(group, node) {
    const {kind, w, h, label, body} = node;
    const centered = kind === "decision";
    const hasIcon = !centered && w >= 150;
    const inset = centered ? w * .19 : 14;
    const textX = centered ? w / 2 : inset + (hasIcon ? 26 : 0);
    const cap = kind === "database" ? Math.min(10, h * .16) : 0;
    const hasBody = !!body.trim() && h - cap * 2 >= (centered ? 70 : 48) && w >= 96;
    const titleY = h / 2 + (hasBody ? -2 : 5) + (kind === "database" ? cap * .32 : 0);
    const titleWidth = centered ? w * .67 : w - textX - (kind === "note" ? 20 : 12);
    if (hasIcon) {
        const paths = {
            process: "M2 4h14v10H2zM5 8h8",
            service: "M2 3h14v5H2zM2 10h14v5H2zM5 5.5h1m-1 7h1",
            database: "M2 5c0-3 14-3 14 0s-14 3-14 0Zm0 0v8c0 3 14 3 14 0V5",
            note: "M4 2h8l3 3v11H4zM11 2v4h4M7 10h5",
        };
        group.append(graphic("path", {
            class: "canvas-node-icon", d: paths[kind], transform: `translate(${inset} ${titleY - 12})`,
            fill: "none", stroke: "#c0c0c0", "stroke-width": 1.35, "stroke-linecap": "round", "stroke-linejoin": "round",
        }));
    }
    group.append(graphic("text", {
        class: "canvas-node-label", x: textX, y: titleY, fill: "#f0f0f0",
        "font-size": 14, "font-weight": 600, "font-family": fontFamily, "text-anchor": centered ? "middle" : "start",
    }, narrowText(label, titleWidth, 14, 600)));
    if (hasBody) {
        const bodyWidth = centered ? Math.max(12, w * (.89 - 34 / h)) : w - textX - (node.source ? 24 : 12);
        group.append(graphic("text", {
            class: "canvas-node-body", x: textX, y: titleY + 18,
            fill: "#b7b7b7", "font-size": 12, "font-family": fontFamily, "text-anchor": centered ? "middle" : "start",
        }, narrowText(body, bodyWidth, 12, 400)));
    }
}

function drawNode(node, selection, trace) {
    const selected = node.id === selection;
    const traced = trace?.nodes.has(node.id);
    const group = groupFor(node, "node", selection, trace, kindNames[node.kind] + ", " + node.label);
    group.setAttribute("data-kind", node.kind);
    group.setAttribute("transform", `translate(${node.x} ${node.y})`);
    group.append(graphic("title", {}, node.label + (node.body ? "\n" + node.body : "")));
    const shape = nodeShape(node);
    shape.setAttribute("class", "canvas-node-shape");
    shape.setAttribute("fill", selected ? "#282828" : "#202020");
    shape.setAttribute("stroke", selected ? "#f2f2f2" : traced ? "#cfcfcf" : "#636363");
    shape.setAttribute("stroke-width", selected ? "1.8" : "1.15");
    group.append(shape);
    nodeDetails(group, node);
    nodeContents(group, node);
    return group;
}

function curve(a, b, reciprocal) {
    const ax = a.x + a.w / 2, ay = a.y + a.h / 2;
    const bx = b.x + b.w / 2, by = b.y + b.h / 2;
    if (a.id === b.id) {
        return [[a.x + a.w, ay], [a.x + a.w + Math.max(52, a.w * .8), ay],
            [ax, a.y - Math.max(52, a.h * .9)], [ax, a.y]];
    }
    const horizontal = Math.abs(bx - ax) / (a.w + b.w) >= Math.abs(by - ay) / (a.h + b.h);
    const direction = Math.sign(horizontal ? bx - ax : by - ay) || 1;
    const dx = horizontal ? direction : 0, dy = horizontal ? 0 : direction;
    const start = [ax + dx * a.w / 2, ay + dy * a.h / 2];
    const end = [bx - dx * b.w / 2, by - dy * b.h / 2];
    const reach = Math.max(24, Math.min(160, Math.hypot(end[0] - start[0], end[1] - start[1]) * .42));
    const offset = reciprocal ? 18 : 0;
    return [start, [start[0] + dx * reach - dy * offset, start[1] + dy * reach + dx * offset],
        [end[0] - dx * reach - dy * offset, end[1] - dy * reach + dx * offset], end];
}

function applyCurve(group, points) {
    const [a, c1, c2, b] = points;
    const d = `M${a} C${c1} ${c2} ${b}`;
    for (const path of group.querySelectorAll("path")) path.setAttribute("d", d);
    const label = group.querySelector("text");
    if (label) {
        label.setAttribute("x", (a[0] + 3*c1[0] + 3*c2[0] + b[0]) / 8);
        label.setAttribute("y", (a[1] + 3*c1[1] + 3*c2[1] + b[1]) / 8 - 7);
    }
}

function drawEdge(edge, a, b, reciprocal, selection, trace) {
    const ink = edge.id === selection || trace?.edges.has(edge.id) ? "active" : "normal";
    const description = (edge.label ? edge.label + ", " : "") + a.label + " → " + b.label;
    const group = groupFor(edge, "edge", selection, trace, description);
    group.append(graphic("title", {}, description));
    group.append(graphic("path", {
        class: "canvas-hit", fill: "none", stroke: "transparent", "stroke-width": 16, "pointer-events": "stroke",
    }));
    group.append(graphic("path", {
        class: "canvas-edge-path", fill: "none", stroke: ink === "active" ? "#ededed" : "#949494",
        "stroke-width": ink === "active" ? 2.3 : 1.5, "marker-end": `url(#canvas-arrow-${ink})`, "pointer-events": "stroke",
    }));
    if (edge.label) {
        group.append(graphic("text", {
            class: "canvas-edge-label", fill: "#d4d4d4", stroke: "#161616", "stroke-width": 4, "stroke-linejoin": "round",
            "paint-order": "stroke", "text-anchor": "middle", "font-family": fontFamily, "font-size": 12,
        }, narrowText(edge.label, 150, 12, 400)));
    }
    applyCurve(group, curve(a, b, reciprocal));
    return group;
}

function drawStroke(stroke, selection) {
    const selected = stroke.id === selection;
    const group = groupFor(stroke, "stroke", selection, null, "Pen stroke");
    group.append(graphic("title", {}, "Pen stroke"));
    // A zero-length first segment gives a tapped point a round cap.
    const points = stroke.points.length === 1 ? [stroke.points[0], stroke.points[0]] : stroke.points;
    const d = "M" + points.join(" L");
    group.append(graphic("path", {class: "canvas-hit", d, fill: "none", stroke: "transparent", "stroke-width": 14, "pointer-events": "stroke", "stroke-linecap": "round"}));
    group.append(graphic("path", {
        class: "canvas-stroke-path", d, fill: "none", stroke: selected ? "#fff" : "#c6c6c6",
        "stroke-width": selected ? 3 : 2.2, "stroke-linejoin": "round", "stroke-linecap": "round",
    }));
    return group;
}

function arrowDefs() {
    const defs = graphic("defs");
    for (const [name, ink] of [["normal", "#949494"], ["active", "#ededed"]]) {
        const marker = graphic("marker", {
            id: "canvas-arrow-" + name, viewBox: "0 0 8 8", refX: 7.5, refY: 4,
            markerWidth: 8, markerHeight: 8, orient: "auto", markerUnits: "userSpaceOnUse",
        });
        marker.append(graphic("path", {d: "M0.8,0.8 L7.5,4 L0.8,7.2", fill: "none", stroke: ink, "stroke-width": 1.4, "stroke-linecap": "round", "stroke-linejoin": "round"}));
        defs.append(marker);
    }
    return defs;
}

export function renderDocument(world, doc, selection = null, trace = null) {
    const previousFocus = document.activeElement?.closest?.("[data-item]");
    const focusedId = previousFocus && world.contains(previousFocus) ? previousFocus.getAttribute("data-item") : null;
    const nodes = new Map(doc.nodes.map(n => [n.id, n]));
    const directions = new Set(doc.edges.map(e => e.from + "/" + e.to));
    const fragment = document.createDocumentFragment();
    const edges = new Map();
    fragment.append(arrowDefs());
    for (const e of doc.edges) {
        const a = nodes.get(e.from), b = nodes.get(e.to);
        if (!a || !b) continue;
        const reciprocal = directions.has(e.to + "/" + e.from);
        const group = drawEdge(e, a, b, reciprocal, selection, trace);
        edges.set(e.id, {group, reciprocal});
        fragment.append(group);
    }
    for (const stroke of doc.strokes) fragment.append(drawStroke(stroke, selection));
    for (const node of doc.nodes) fragment.append(drawNode(node, selection, trace));
    world.replaceChildren(fragment);
    layersFor.set(world, {nodes, edges});
    if (focusedId) {
        for (const item of world.querySelectorAll("[data-item]")) {
            if (item.getAttribute("data-item") === focusedId) {
                item.focus({preventScroll: true});
                break;
            }
        }
    }
}

// During a drag, only the connected curves move with the node until pointerup commits it.
export function updateIncidentEdges(world, doc, movedId, x, y) {
    const scene = layersFor.get(world);
    if (!scene) return;
    const node = scene.nodes.get(movedId);
    if (!node) return;
    const moved = {...node, x, y};
    for (const edge of doc.edges) {
        if (edge.from !== movedId && edge.to !== movedId) continue;
        const rendered = scene.edges.get(edge.id);
        if (!rendered) continue;
        const a = edge.from === movedId ? moved : scene.nodes.get(edge.from);
        const b = edge.to === movedId ? moved : scene.nodes.get(edge.to);
        if (a && b) applyCurve(rendered.group, curve(a, b, rendered.reciprocal));
    }
}

export function mountInspector(api) {
    const {host, root} = api;
    const uuid = /^[\da-f]{8}-[\da-f]{4}-[\da-f]{4}-[\da-f]{4}-[\da-f]{12}$/i;
    const drafts = new Map();
    let session = null;
    let updating = false;

    function element(tag, className, text) {
        const result = document.createElement(tag);
        if (className) result.className = className;
        if (text != null) result.textContent = text;
        return result;
    }

    function findItem(doc, id) {
        if (!id) return null;
        for (const group of ["nodes", "edges", "strokes"]) {
            const item = doc[group].find(record => record.id === id);
            if (item) return {group, item};
        }
        return null;
    }

    function saveField(editor, field) {
        clearTimeout(field.timer);
        const board = api.getBoard();
        const item = board.document[editor.group].find(record => record.id === editor.id);
        if (board.read_only || api.canEdit?.() === false || !item) return false;
        const next = field.input.value;
        if (item[field.key] !== field.base && next !== item[field.key]) {
            field.conflict.hidden = false;
            return false;
        }
        const base = field.base;
        field.base = next;
        field.conflict.hidden = true;
        drafts.delete(field.draftKey);
        if (next !== item[field.key]) {
            const applied = api.change(doc => {
                const target = doc[editor.group].find(record => record.id === editor.id);
                if (target) target[field.key] = next;
            });
            if (applied === false) {
                field.base = base;
                drafts.set(field.draftKey, {base, value: next});
                return false;
            }
        }
        return true;
    }

    function commitPending() {
        if (!session) return true;
        let ok = true;
        const editor = session;
        for (const field of editor.fields) {
            if (drafts.has(field.draftKey) && !saveField(editor, field)) ok = false;
        }
        return ok;
    }

    function discardPending() {
        drafts.clear();
        if (session) {
            for (const field of session.fields) {
                field.input.value = field.base;
                field.input.setCustomValidity("");
                field.conflict.hidden = true;
            }
        }
    }

    function addField(editor, form, key, caption, value) {
        const label = element("label", "canvas-inspector-field");
        const input = element(key === "body" ? "textarea" : "input");
        if (key === "body") input.rows = 3;
        else input.type = "text";
        input.name = key;
        input.maxLength = key === "body" ? 2000 : 160;
        const draftKey = editor.id + "/" + key;
        const draft = drafts.get(draftKey);
        input.value = draft?.value ?? value;
        label.append(element("span", "", caption), input);
        const conflict = element("div", "canvas-inspector-conflict");
        conflict.hidden = true;
        conflict.append(element("span", "", caption + " changed on this board."));
        const boardVersion = element("button", "action-quiet", "Use saved text");
        boardVersion.type = "button";
        const mine = element("button", "action-secondary", "Apply my edit");
        mine.type = "button";
        conflict.append(boardVersion, mine);
        const field = {key, input, conflict, draftKey, base: draft?.base ?? value};
        editor.fields.push(field);
        function takeCurrent() {
            const current = api.getBoard().document[editor.group].find(record => record.id === editor.id);
            if (!current) return;
            field.base = current[key];
            input.value = field.base;
            input.setCustomValidity("");
            conflict.hidden = true;
            drafts.delete(draftKey);
        }
        boardVersion.addEventListener("click", takeCurrent);
        mine.addEventListener("click", () => {
            const current = api.getBoard().document[editor.group].find(record => record.id === editor.id);
            if (!current) return;
            field.base = current[key];
            saveField(editor, field);
        });
        function stageInput(event) {
            clearTimeout(field.timer);
            input.setCustomValidity("");
            if (input.value === field.base) drafts.delete(draftKey);
            else {
                drafts.set(draftKey, {base: field.base, value: input.value});
                if (!event.isComposing) field.timer = setTimeout(() => {
                    if (session === editor && drafts.has(draftKey)) saveField(editor, field);
                }, 650);
            }
        }
        input.addEventListener("input", stageInput);
        input.addEventListener("compositionend", stageInput);
        input.addEventListener("change", () => saveField(editor, field));
        input.addEventListener("keydown", event => {
            if (event.isComposing) return;
            if (event.key === "Escape") {
                event.stopPropagation();
                takeCurrent();
                root.querySelector("#canvas-stage")?.focus();
            } else if (event.key === "Enter" && (key !== "body" || event.ctrlKey || event.metaKey)) {
                event.preventDefault();
                if (saveField(editor, field)) root.querySelector("#canvas-stage")?.focus();
            }
        });
        form.append(label, conflict);
    }

    function makeSession(group, item) {
        const editor = {group, id: item.id, fields: []};
        const heading = element("div", "canvas-inspector-heading");
        editor.heading = element("h2");
        const close = element("button", "canvas-inspector-close", "×");
        close.type = "button";
        close.setAttribute("aria-label", "Close inspector");
        close.title = "Close inspector";
        close.addEventListener("click", () => {
            if (!commitPending()) return;
            api.select(null);
            root.querySelector("#canvas-stage")?.focus();
        });
        heading.append(editor.heading, close);
        const form = element("form", "canvas-inspector-form");
        form.addEventListener("submit", e => { e.preventDefault(); commitPending(); });
        if (group === "nodes") {
            addField(editor, form, "label", "Title", item.label);
            addField(editor, form, "body", "Notes", item.body);
            editor.source = element("a", "canvas-inspector-source");
            editor.source.addEventListener("click", async event => {
                if (event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.button !== 0 || !api.flush) return;
                event.preventDefault();
                const destination = editor.source.href;
                if (commitPending() && await api.flush()) window.location.assign(destination);
            });
            form.append(editor.source);
        } else if (group === "edges") {
            addField(editor, form, "label", "Label", item.label);
            editor.endpoints = element("p", "canvas-inspector-endpoints");
            form.append(editor.endpoints);
        } else {
            editor.measure = element("p", "canvas-inspector-measure");
            form.append(editor.measure);
        }
        host.replaceChildren(heading, form);
        return editor;
    }

    function update() {
        if (updating) return;
        updating = true;
        try {
            let board = api.getBoard();
            let selected = findItem(board.document, api.getSelection());
            if (session && (selected?.item.id !== session.id || selected?.group !== session.group)) {
                // Pointer selection can change before a focused input's native change event.
                commitPending();
                board = api.getBoard();
                selected = findItem(board.document, api.getSelection());
                session = null;
            }
            host.hidden = !selected;
            if (!selected) {
                host.replaceChildren();
                return;
            }
            const {group, item} = selected;
            if (!session) session = makeSession(group, item);
            session.heading.textContent = group === "nodes" ? kindNames[item.kind] : group === "edges" ? "Connection" : "Freehand stroke";
            for (const field of session.fields) {
                const draft = drafts.get(field.draftKey);
                field.input.readOnly = board.read_only || api.canEdit?.() === false;
                if (!draft) {
                    field.base = item[field.key];
                    if (field.input.value !== field.base) field.input.value = field.base;
                }
                field.conflict.hidden = !draft || draft.base === item[field.key] || field.input.readOnly;
            }
            if (group === "nodes") {
                const source = item.source;
                const valid = source && (source.kind === "issue" || source.kind === "note") && uuid.test(source.id);
                session.source.hidden = !valid;
                if (valid) {
                    session.source.href = (source.kind === "issue" ? "/issues/" : "/notes/") + source.id;
                    session.source.textContent = "Open linked " + source.kind;
                } else session.source.removeAttribute("href");
            } else if (group === "edges") {
                const from = board.document.nodes.find(n => n.id === item.from);
                const to = board.document.nodes.find(n => n.id === item.to);
                session.endpoints.textContent = (from?.label ?? "") + " → " + (to?.label ?? "");
            } else {
                session.measure.textContent = item.points.length + (item.points.length === 1 ? " point" : " points");
            }
        } finally {
            updating = false;
        }
    }

    function editSelection() {
        update();
        const field = session?.fields[0]?.input;
        if (field) { field.focus(); field.select(); }
    }

    window.addEventListener("beforeunload", event => {
        if (drafts.size) { event.preventDefault(); event.returnValue = ""; }
    });
    return {update, editSelection, commitPending, discardPending};
}
