const ns = "http://www.w3.org/2000/svg";
let serial = 0;

function html(tag, className, text) {
    const el = document.createElement(tag);
    if (className) el.className = className;
    if (text !== undefined) el.textContent = text;
    return el;
}

function vector(tag, attributes) {
    const el = document.createElementNS(ns, tag);
    for (const [name, value] of Object.entries(attributes)) el.setAttribute(name, value);
    return el;
}

function folded(value) {
    return value.normalize("NFKD").replace(/\p{M}/gu, "").toLowerCase();
}

function resultsFor(nodes, query) {
    const phrase = folded(query.trim());
    if (!phrase) return nodes;
    const terms = phrase.split(/\s+/);
    const matches = [];
    for (const node of nodes) {
        const title = folded(node.label);
        const text = title + " " + folded(node.body);
        if (!terms.every(term => text.includes(term))) continue;
        let rank = 3;
        if (title === phrase) rank = 0;
        else if (title.startsWith(phrase)) rank = 1;
        else if (terms.every(term => title.includes(term))) rank = 2;
        matches.push({node, rank});
    }
    return matches.sort((left, right) => left.rank - right.rank).map(match => match.node);
}

function displayName(node) {
    return node.label.trim() || "Untitled " + node.kind;
}

export function mountSearch(api, panel, close) {
    const id = "canvas-find-" + ++serial;
    const section = html("div", "canvas-find");
    const input = html("input", "canvas-find-input");
    input.type = "search";
    input.id = id;
    input.placeholder = "Find a node…";
    input.autocomplete = "off";
    input.spellcheck = false;
    input.setAttribute("role", "combobox");
    input.setAttribute("aria-label", "Find a node by title or text");
    input.setAttribute("aria-controls", id + "-results");
    input.setAttribute("aria-autocomplete", "list");
    const status = html("p", "canvas-find-status");
    status.setAttribute("role", "status");
    const list = html("div", "canvas-find-results");
    list.id = id + "-results";
    list.setAttribute("role", "listbox");
    list.setAttribute("aria-label", "Matching nodes");
    section.append(input, status, list);
    panel.append(section);
    let active = -1;
    let results = [];
    let lastDocument;

    function mark(index, scroll) {
        list.children[active]?.setAttribute("aria-selected", "false");
        active = index;
        const row = list.children[index];
        if (!row) {
            input.removeAttribute("aria-activedescendant");
            return;
        }
        row.setAttribute("aria-selected", "true");
        input.setAttribute("aria-activedescendant", row.id);
        if (scroll) row.scrollIntoView({block: "nearest", inline: "nearest"});
    }

    function render(preferred) {
        lastDocument = api.getBoard().document;
        results = resultsFor(lastDocument.nodes, input.value);
        const rows = document.createDocumentFragment();
        for (const [i, node] of results.entries()) {
            const row = html("button", "canvas-find-result");
            row.type = "button";
            row.tabIndex = -1;
            row.id = id + "-result-" + i;
            row.dataset.node = node.id;
            row.setAttribute("role", "option");
            row.setAttribute("aria-selected", "false");
            row.append(html("span", "canvas-find-name", displayName(node)), html("small", "", node.kind));
            rows.append(row);
        }
        list.replaceChildren(rows);
        status.textContent = results.length
            ? results.length + (results.length === 1 ? " match" : " matches")
            : (lastDocument.nodes.length ? "No matching nodes" : "No nodes on this canvas");
        input.setAttribute("aria-expanded", String(results.length > 0));
        const index = preferred ? results.findIndex(node => node.id === preferred) : -1;
        mark(results.length > 0 ? Math.max(index, 0) : -1, false);
        list.scrollTop = 0;
    }

    function jump(id) {
        const node = api.getBoard().document.nodes.find(item => item.id === id);
        if (!node) { render(); return; }
        if (close) close('selection');
        api.select(node.id);
        api.centerOn(node.x + node.w / 2, node.y + node.h / 2);
    }

    list.addEventListener("click", event => {
        const row = event.target.closest("[data-node]");
        if (row && list.contains(row)) jump(row.dataset.node);
    });
    input.addEventListener("input", () => render());
    input.addEventListener("keydown", event => {
        if (event.isComposing || event.altKey || event.metaKey || event.ctrlKey) return;
        if (event.key === "Escape" && close) {
            event.preventDefault();
            event.stopPropagation();
            close();
            return;
        }
        if (!results.length) return;
        if (event.key === "Enter") {
            event.preventDefault();
            event.stopPropagation();
            if (results[active]) jump(results[active].id);
        } else if (event.key === "ArrowUp" || event.key === "ArrowDown") {
            event.preventDefault();
            event.stopPropagation();
            const delta = event.key === "ArrowUp" ? -1 : 1;
            mark((active + delta + results.length) % results.length, true);
        }
    });
    render(api.getSelection());
    input.focus({preventScroll: true});
    const unsubscribe = api.onChange(() => {
        if (api.getBoard().document !== lastDocument) render(results[active]?.id);
    });
    return () => {
        unsubscribe();
        section.remove();
    };
}

const mapWidth = 184;
const mapHeight = 100;
const mapInset = 6;

function emptyBounds() {
    return {left: Infinity, top: Infinity, right: -Infinity, bottom: -Infinity};
}

function include(bounds, x, y) {
    bounds.left = Math.min(bounds.left, x);
    bounds.top = Math.min(bounds.top, y);
    bounds.right = Math.max(bounds.right, x);
    bounds.bottom = Math.max(bounds.bottom, y);
}

function mapGeometry(doc) {
    const drawing = document.createDocumentFragment();
    const marks = new Map();
    const nodes = new Map(doc.nodes.map(node => [node.id, node]));
    const bounds = emptyBounds();
    for (const wire of doc.edges) {
        const a = nodes.get(wire.from);
        const b = nodes.get(wire.to);
        if (!a || !b) continue;
        const line = vector("line", {x1: a.x + a.w/2, y1: a.y + a.h/2, x2: b.x + b.w/2, y2: b.y + b.h/2, class: "canvas-map-line"});
        drawing.append(line);
        marks.set(wire.id, line);
    }
    for (const stroke of doc.strokes) {
        const points = stroke.points;
        if (!points.length) continue;
        for (const [x,y] of points) include(bounds, x, y);
        // All points affect the map extents. A small representative outline is enough at overview scale.
        const step = Math.max(1, Math.ceil(points.length / 180));
        const outline = points.filter((_, i) => i % step === 0 || i === points.length - 1);
        const mark = vector("polyline", {
            class: "canvas-map-line canvas-map-stroke",
            points: outline.map(([x,y]) => x + "," + y).join(" ")
        });
        drawing.append(mark);
        marks.set(stroke.id, mark);
    }
    for (const node of doc.nodes) {
        include(bounds, node.x, node.y);
        include(bounds, node.x + node.w, node.y + node.h);
        const mark = vector("rect", {x: node.x, y: node.y, width: node.w, height: node.h, class: "canvas-map-node"});
        drawing.append(mark);
        marks.set(node.id, mark);
    }
    return {drawing, marks, bounds};
}

function mapProjection(content, view) {
    const bounds = {...content};
    include(bounds, view.x, view.y);
    include(bounds, view.x + view.width, view.y + view.height);
    const w = Math.max(1, bounds.right - bounds.left);
    const h = Math.max(1, bounds.bottom - bounds.top);
    const scale = Math.min((mapWidth - mapInset * 2) / w, (mapHeight - mapInset * 2) / h);
    return {
        scale,
        x: mapWidth/2 - (bounds.left + w/2)*scale,
        y: mapHeight/2 - (bounds.top + h/2)*scale
    };
}

export function mountMinimap(api, host) {
    const id = "canvas-map-" + ++serial;
    const toggle = html("button", "canvas-map-toggle");
    toggle.type = "button";
    toggle.setAttribute("aria-controls", id);
    const icon = vector("svg", {viewBox:"0 0 16 16", width:14, height:14, "aria-hidden":"true"});
    icon.append(vector("path", {d:"M1.75 3.25h12.5v9.5H1.75z M4 5.5h4.25v3H4z M9.6 9.25h2.5", fill:"none", stroke:"currentColor", "stroke-width":"1.2", "stroke-linejoin":"round"}));
    toggle.append(icon, html("span", "", "Map"));
    const surface = vector("svg", {
        id, viewBox: "0 0 " + mapWidth + " " + mapHeight, class: "canvas-map-surface",
        tabindex: "0", role: "group", "aria-label": "Map overview", "aria-describedby": id + "-help",
        "aria-keyshortcuts": "ArrowUp ArrowDown ArrowLeft ArrowRight Home Enter"
    });
    const help = html("span", "sr-only", "Click to center the canvas, use the arrow keys to pan, or press Enter to fit the drawing.");
    help.id = id + "-help";
    const world = vector("g", {"aria-hidden":"true"});
    const viewport = vector("rect", {class:"canvas-map-viewport", "aria-hidden":"true"});
    surface.append(viewport, world);
    host.replaceChildren(toggle, surface, help);
    const compact = window.matchMedia("(max-width: 700px)");
    let open = !compact.matches;
    let manual = false;
    let previousDoc;
    let marks;
    let selected;
    let bounds;
    let projection;
    let frame = 0;

    function redraw() {
        frame = 0;
        const doc = api.getBoard().document;
        host.hidden = doc.nodes.length === 0 && doc.strokes.length === 0;
        if (host.hidden || !open) return;
        if (doc !== previousDoc) {
            const geometry = mapGeometry(doc);
            previousDoc = doc;
            bounds = geometry.bounds;
            marks = geometry.marks;
            world.replaceChildren(geometry.drawing);
            selected = null;
        }
        const selection = api.getSelection();
        if (selected !== selection) {
            marks.get(selected)?.classList.remove("is-selected");
            marks.get(selection)?.classList.add("is-selected");
            selected = selection;
        }
        const view = api.getView();
        if (!view || !(view.width > 0 && view.height > 0)) return;
        projection = mapProjection(bounds, view);
        const {x, y, scale} = projection;
        world.setAttribute("transform", "translate(" + x + " " + y + ") scale(" + scale + ")");
        viewport.setAttribute("x", x + view.x * scale);
        viewport.setAttribute("y", y + view.y * scale);
        viewport.setAttribute("width", view.width * scale);
        viewport.setAttribute("height", view.height * scale);
    }

    function syncOpen() {
        toggle.setAttribute("aria-expanded", String(open));
        toggle.setAttribute("aria-label", open ? "Hide map overview" : "Show map overview");
        toggle.title = toggle.getAttribute("aria-label");
        surface.style.display = open ? "" : "none";
        redraw();
    }
    function onCompact() {
        if (!manual) { open = !compact.matches; syncOpen(); }
    }
    compact.addEventListener("change", onCompact);
    toggle.addEventListener("click", () => {
        manual = true;
        open = !open;
        syncOpen();
    });
    surface.addEventListener("click", event => {
        const matrix = surface.getScreenCTM();
        if (!matrix || !projection) return;
        const point = new DOMPoint(event.clientX, event.clientY).matrixTransform(matrix.inverse());
        api.centerOn((point.x - projection.x) / projection.scale, (point.y - projection.y) / projection.scale);
        surface.focus({preventScroll: true});
    });
    surface.addEventListener("keydown", event => {
        if (event.altKey || event.ctrlKey || event.metaKey) return;
        const v = api.getView();
        if (!v) return;
        const step = event.shiftKey ? 0.75 : 0.25;
        let x = v.x + v.width / 2, y = v.y + v.height / 2;
        switch (event.key) {
            case "ArrowUp": y -= v.height * step; break;
            case "ArrowDown": y += v.height * step; break;
            case "ArrowLeft": x -= v.width * step; break;
            case "ArrowRight": x += v.width * step; break;
            case "Home":
            case "Enter": event.preventDefault(); event.stopPropagation(); api.fit(); return;
            default: return;
        }
        event.preventDefault();
        event.stopPropagation();
        api.centerOn(x, y);
    });
    syncOpen();
    const unsubscribe = api.onChange(() => {
        if (!frame) frame = requestAnimationFrame(redraw);
    });
    return () => {
        unsubscribe();
        compact.removeEventListener("change", onCompact);
        cancelAnimationFrame(frame);
        host.replaceChildren();
    };
}
