import { updateIncidentEdges } from "./canvas-render.js";

const NS = "http://www.w3.org/2000/svg";
const GRID = 16;
const LIMIT = 16000;
const MIN_ZOOM = 1 / 512;
const SHAPES = {
    process: ["Process", 176, 80],
    service: ["Service", 176, 80],
    note: ["Note", 176, 80],
    database: ["Database", 152, 88],
    decision: ["Decision", 176, 104],
};

const clamp = (value, lo, hi) => Math.min(hi, Math.max(lo, value));
const distance = (a, b) => Math.hypot(a[0] - b[0], a[1] - b[1]);
const midpoint = (a, b) => [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2];
const rounded = value => Math.round(value * 10) / 10;

export function mountGestures(api) {
    const {stage, world} = api;
    const root = stage.closest("[data-canvas-editor]") || stage.parentElement;
    let tool = "select";
    let cx = 0, cy = 0, zoom = 1;
    let gesture = null;
    let from = null;
    let preview = null;
    let lastTap = null;
    let spacePan = false;
    let viewportFrame = 0;
    const active = new Map();
    const data = () => api.getBoard().document;
    const canEdit = () => !api.getBoard().read_only && (api.canEdit?.() ?? true);
    const nodeById = id => data().nodes.find(node => node.id === id);
    const snap = value => api.isSnapped() ? Math.round(value / GRID) * GRID : rounded(value);

    function getView() {
        return {
            x: cx - (stage.clientWidth || 1) / (2 * zoom),
            y: cy - (stage.clientHeight || 1) / (2 * zoom),
            width: (stage.clientWidth || 1) / zoom,
            height: (stage.clientHeight || 1) / zoom,
        };
    }

    function paintView() {
        const {x, y, width, height} = getView();
        stage.setAttribute("viewBox", [x, y, width, height].join(" "));
        // Retain a legible grid at both extremes. Every visible dot still lies on the logical grid.
        let spacing = GRID * zoom;
        while (spacing < 9) spacing *= 2;
        stage.style.setProperty("--canvas-grid-size", spacing + "px");
        // The radial dot sits in the center of each background tile.
        stage.style.setProperty("--canvas-grid-x", ((-x * zoom) % spacing - spacing / 2) + "px");
        stage.style.setProperty("--canvas-grid-y", ((-y * zoom) % spacing - spacing / 2) + "px");
        if (!viewportFrame) viewportFrame = requestAnimationFrame(() => {
            viewportFrame = 0;
            api.changedView();
        });
    }

    function centerOn(x, y) {
        if (!Number.isFinite(x) || !Number.isFinite(y)) return;
        cx = x;
        cy = y;
        paintView();
    }

    function fit() {
        let left = Infinity, right = -Infinity, top = Infinity, bottom = -Infinity;
        function include(x, y) {
            left = Math.min(left, x); right = Math.max(right, x);
            top = Math.min(top, y); bottom = Math.max(bottom, y);
        }
        for (const node of data().nodes) {
            include(node.x, node.y);
            include(node.x + node.w, node.y + node.h);
        }
        for (const stroke of data().strokes) {
            for (const point of stroke.points) include(point[0], point[1]);
        }
        if (!Number.isFinite(left)) {
            zoom = 1;
            centerOn(0, 0);
            return;
        }
        const w = stage.clientWidth || 1, h = stage.clientHeight || 1;
        const padding = Math.min(w * .1, h * .1, 64);
        zoom = clamp(Math.min((w - 2 * padding) / Math.max(right - left, 1),
            (h - 2 * padding) / Math.max(bottom - top, 1)), MIN_ZOOM, 1);
        centerOn((left + right) / 2, (top + bottom) / 2);
    }

    function initialView() {
        fit();
        if (stage.clientWidth >= 640 || zoom >= .85 || !data().nodes.length) return;
        // On a phone a large board fits as a thumbnail. Open on a readable
        // starting node; Fit drawing and the minimap retain the full overview.
        const doc = data();
        const anchor = doc.nodes.find(node => node.source) || doc.nodes[0];
        const edge = doc.edges.find(edge => edge.from === anchor.id || edge.to === anchor.id);
        const neighbor = edge && nodeById(edge.from === anchor.id ? edge.to : edge.from);
        zoom = 1;
        const x = anchor.x + anchor.w / 2, y = anchor.y + anchor.h / 2;
        const allowance = Math.max(0, Math.min(48, (stage.clientWidth - anchor.w) / 2 - 12));
        const dx = neighbor ? clamp((neighbor.x + neighbor.w / 2 - x) / 3, -allowance, allowance) : 0;
        const dy = neighbor ? clamp((neighbor.y + neighbor.h / 2 - y) / 3, -42, 42) : 0;
        centerOn(x + dx, y + dy);
    }

    function toWorld(client) {
        const rect = stage.getBoundingClientRect();
        return [cx + (client[0] - rect.left - rect.width / 2) / zoom,
            cy + (client[1] - rect.top - rect.height / 2) / zoom];
    }

    function zoomAt(nextZoom, client, anchor) {
        zoom = clamp(nextZoom, MIN_ZOOM, 4);
        const moved = toWorld(client);
        centerOn(cx + anchor[0] - moved[0], cy + anchor[1] - moved[1]);
    }

    function zoomBy(factor) {
        if (!Number.isFinite(factor) || factor <= 0) return;
        zoom = clamp(zoom * factor, MIN_ZOOM, 4);
        paintView();
    }

    function clearPreview() {
        preview?.remove();
        preview = null;
    }

    function makePreview(kind) {
        clearPreview();
        preview = document.createElementNS(NS, kind);
        preview.setAttribute("fill", "none");
        preview.setAttribute("stroke", "currentColor");
        preview.setAttribute("stroke-width", "2");
        preview.setAttribute("stroke-linecap", "round");
        preview.setAttribute("stroke-linejoin", "round");
        preview.setAttribute("pointer-events", "none");
        preview.setAttribute("aria-hidden", "true");
        stage.append(preview);
        return preview;
    }

    function clearGesture() {
        if (gesture?.type === "drag" && gesture.moved) api.render();
        gesture = null;
        clearPreview();
        delete stage.dataset.dragging;
    }

    function cancel() {
        clearGesture();
        from = null;
        lastTap = null;
        spacePan = false;
        for (const id of active.keys()) {
            if (stage.hasPointerCapture(id)) stage.releasePointerCapture(id);
        }
        active.clear();
    }

    function setTool(name) {
        if (!Object.hasOwn(SHAPES, name) && !["select", "pan", "connect", "pen"].includes(name)) return;
        if (!canEdit() && !["select", "pan"].includes(name)) return;
        cancel();
        tool = name;
        root.dataset.tool = stage.dataset.tool = name;
        for (const button of root.querySelectorAll("button[data-tool]")) {
            button.setAttribute("aria-pressed", String(button.dataset.tool === name));
        }
        const picker = root.querySelector(".canvas-shapes");
        if (picker) picker.open = false;
        if (name === "connect") api.notify("Choose the first shape");
    }

    function addNode(kind, [x, y]) {
        if (!canEdit()) return;
        if (data().nodes.length >= 250) return api.notify("This canvas holds up to 250 shapes");
        const [label, w, h] = SHAPES[kind];
        const node = {
            id: crypto.randomUUID(), kind, label, body: "", w, h,
            x: clamp(snap(x - w / 2), -LIMIT, LIMIT - w),
            y: clamp(snap(y - h / 2), -LIMIT, LIMIT - h),
        };
        api.change(doc => doc.nodes.push(node));
        setTool("select");
        api.select(node.id);
    }

    function connect(id) {
        if (!canEdit()) return;
        const target = nodeById(id);
        if (!target) return api.notify(from ? "Choose a destination shape" : "Choose the first shape");
        if (!from || !nodeById(from)) {
            from = id;
            api.select(id);
            api.notify("Choose a destination shape");
            return;
        }
        if (from === id) return api.notify("Choose a different destination");
        if (data().edges.length >= 500) return api.notify("This canvas holds up to 500 connections");
        if (data().edges.some(edge => edge.from === from && edge.to === id)) {
            return api.notify("These shapes already have that connection");
        }
        const edge = {id: crypto.randomUUID(), from, to: id, label: ""};
        api.change(doc => doc.edges.push(edge));
        setTool("select");
        api.select(edge.id);
    }

    function connectorGuide(point, source = from) {
        const node = nodeById(source);
        if (!node) return;
        const line = preview || makePreview("line");
        line.setAttribute("stroke-dasharray", "5 5");
        line.setAttribute("x1", node.x + node.w / 2);
        line.setAttribute("y1", node.y + node.h / 2);
        line.setAttribute("x2", point[0]);
        line.setAttribute("y2", point[1]);
    }

    function addPoint(trace, point, isLast = false) {
        const p = point.map(v => rounded(clamp(v, -LIMIT, LIMIT)));
        const last = trace.points.at(-1);
        if (last && distance(last, p) < (isLast ? .01 : 1.5 / zoom)) return;
        // Downsample in place so long strokes keep their beginning and their full drawn extent.
        if (trace.points.length >= trace.limit) {
            trace.points = trace.points.filter((_, i) => i % 2 === 0);
        }
        trace.points.push(p);
    }

    function showInk(trace) {
        (preview || makePreview("polyline")).setAttribute("points", trace.points.map(p => p.join(",")).join(" "));
    }

    function captured(id) {
        try { stage.setPointerCapture(id); } catch { /* Pointer may already have ended. */ }
    }

    function pointerDown(event) {
        if (event.button !== 0 && event.button !== 1) return;
        if (active.size >= 2) return;
        const client = [event.clientX, event.clientY];
        const point = toWorld(client);
        const id = event.target.closest?.("[data-item]")?.dataset.item ?? null;
        active.set(event.pointerId, client);
        captured(event.pointerId);
        event.preventDefault();
        stage.focus({preventScroll: true});
        if (active.size === 2) {
            clearGesture();
            from = null;
            const [a, b] = active.values();
            gesture = {type: "pinch", start: Math.max(1, distance(a, b)), zoom, anchor: toWorld(midpoint(a, b))};
            return;
        }
        if (active.size !== 1) return;
        const pan = tool === "pan" || event.button === 1 || spacePan;
        if (!pan && tool === "pen" && canEdit()) {
            const limit = Math.min(2048, 20000 - data().strokes.reduce((n, s) => n + s.points.length, 0));
            if (data().strokes.length >= 200 || limit < 2) return api.notify("This canvas has reached its ink limit");
            gesture = {type: "ink", pointerId: event.pointerId, points: [], limit};
            addPoint(gesture, point);
            showInk(gesture);
            return;
        }
        const movable = tool === "select" && !pan && canEdit() && id;
        const node = movable && nodeById(id);
        const stroke = movable && !node && data().strokes.find(s => s.id === id);
        let type = "tap";
        if (node || stroke) type = "drag";
        else if (pan || (tool === "select" && !id)) type = "pan";
        else if (tool === "connect" && !from && nodeById(id) && canEdit()) type = "wire";
        gesture = {type, pointerId: event.pointerId, client, point, id, node, stroke,
            cx, cy, moved: false, pan};
        if (stroke) {
            const xs = stroke.points.map(p => p[0]), ys = stroke.points.map(p => p[1]);
            gesture.bounds = [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)];
        }
        if (movable) api.select(id);
    }

    function nodePosition(g, client) {
        const p = toWorld(client);
        return [clamp(snap(g.node.x + p[0] - g.point[0]), -LIMIT, LIMIT - g.node.w),
            clamp(snap(g.node.y + p[1] - g.point[1]), -LIMIT, LIMIT - g.node.h)];
    }

    function strokeDelta(g, client) {
        const p = toWorld(client);
        const [left, top, right, bottom] = g.bounds;
        return [clamp(snap(left + p[0] - g.point[0]) - left, -LIMIT - left, LIMIT - right),
            clamp(snap(top + p[1] - g.point[1]) - top, -LIMIT - top, LIMIT - bottom)];
    }

    function move(event) {
        const client = [event.clientX, event.clientY];
        if (active.has(event.pointerId)) active.set(event.pointerId, client);
        const g = gesture;
        if (!g) {
            if (tool === "connect" && from) connectorGuide(toWorld(client));
            return;
        }
        if (g.type === "pinch" && active.size >= 2) {
            const [a, b] = active.values();
            zoomAt(g.zoom * distance(a, b) / g.start, midpoint(a, b), g.anchor);
            return;
        }
        if (g.pointerId !== event.pointerId) return;
        if (g.type === "ink") {
            const sample = event.getCoalescedEvents?.() || [];
            for (const p of sample) addPoint(g, toWorld([p.clientX, p.clientY]));
            addPoint(g, toWorld(client));
            showInk(g);
            return;
        }
        if (!g.moved && distance(client, g.client) < 3) return;
        g.moved = true;
        if (g.type === "pan") {
            stage.dataset.dragging = "pan";
            centerOn(g.cx - (client[0] - g.client[0]) / zoom, g.cy - (client[1] - g.client[1]) / zoom);
        } else if (g.type === "drag") {
            stage.dataset.dragging = "shape";
            let dx, dy;
            if (g.node) {
                const [x, y] = nodePosition(g, client);
                dx = x - g.node.x; dy = y - g.node.y;
                updateIncidentEdges(world, data(), g.id, x, y);
            } else {
                [dx, dy] = strokeDelta(g, client);
            }
            if (!g.element?.isConnected) {
                g.element = world.querySelector('[data-item="' + g.id + '"]');
                g.transform = g.element?.getAttribute("transform") || "";
            }
            g.element?.setAttribute("transform", "translate(" + dx + " " + dy + ") " + g.transform);
        } else if (g.type === "wire") {
            connectorGuide(toWorld(client), g.id);
        }
    }

    function tap(id, point) {
        if (Object.hasOwn(SHAPES, tool)) return addNode(tool, point);
        if (tool === "connect") return connect(id);
        if (tool !== "select") return;
        const now = performance.now();
        const double = id && lastTap?.id === id && now - lastTap.at < 420;
        api.select(id);
        lastTap = double ? null : {id, at: now};
        if (double) api.editSelection();
    }

    function up(event, aborted = false) {
        if (!active.delete(event.pointerId)) return;
        if (stage.hasPointerCapture(event.pointerId)) stage.releasePointerCapture(event.pointerId);
        const g = gesture;
        if (!g) return;
        if (g.type === "pinch") {
            if (active.size === 1) {
                const [pointerId, client] = active.entries().next().value;
                gesture = {type: "pan", pointerId, client, cx, cy, moved: true, pan: true};
            } else if (!active.size) clearGesture();
            return;
        }
        if (g.pointerId !== event.pointerId) return;
        if (aborted) { clearGesture(); return; }
        gesture = null;
        delete stage.dataset.dragging;
        const client = [event.clientX, event.clientY];
        if (!canEdit() && ["drag", "ink", "wire"].includes(g.type)) {
            api.render();
            clearPreview();
            return;
        }
        // Synthetic input and short touch drags may finish without an intermediate move event.
        if (g.type === "drag" && (g.moved || distance(client, g.client) >= 3)) {
            if (g.node) {
                const [x, y] = nodePosition(g, client);
                if (x !== g.node.x || y !== g.node.y) api.change(doc => {
                    const node = doc.nodes.find(item => item.id === g.id);
                    if (node) { node.x = x; node.y = y; }
                });
                else api.render();
            } else {
                const [dx, dy] = strokeDelta(g, client);
                if (dx || dy) api.change(doc => {
                    const stroke = doc.strokes.find(item => item.id === g.id);
                    if (stroke) stroke.points = stroke.points.map(([x, y]) => [rounded(x + dx), rounded(y + dy)]);
                });
                else api.render();
            }
            lastTap = null;
        } else if (g.type === "wire" && g.moved) {
            clearPreview();
            from = g.id;
            const destination = document.elementFromPoint(...client)?.closest("[data-item]")?.dataset.item;
            connect(destination);
        } else if (g.type === "ink") {
            addPoint(g, toWorld(client), true);
            clearPreview();
            if (g.points.length < 2) {
                const [x, y] = g.points[0];
                g.points.push([x < LIMIT ? x + .1 : x - .1, y]);
            }
            const line = {id: crypto.randomUUID(), points: g.points};
            api.change(doc => doc.strokes.push(line));
        } else if (!g.moved && !g.pan) {
            tap(g.id, toWorld(client));
        }
    }

    stage.addEventListener("pointerdown", pointerDown);
    stage.addEventListener("pointermove", move);
    stage.addEventListener("pointerup", event => up(event));
    stage.addEventListener("pointercancel", event => up(event, true));
    stage.addEventListener("lostpointercapture", event => up(event, true));
    stage.addEventListener("wheel", event => {
        event.preventDefault();
        if (gesture) return;
        const client = [event.clientX, event.clientY];
        const line = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? stage.clientHeight : 1;
        if (event.shiftKey || (!event.ctrlKey && event.deltaX)) {
            const dx = event.deltaX || event.deltaY;
            centerOn(cx + dx * line / zoom, cy + (event.shiftKey ? 0 : event.deltaY * line / zoom));
        } else {
            zoomAt(zoom * Math.exp(clamp(-event.deltaY * line / 480, -2, 2)), client, toWorld(client));
        }
    }, {passive: false});

    stage.addEventListener("keydown", event => {
        if (event.altKey || event.ctrlKey || event.metaKey) return;
        if (event.key === "Escape" && (tool !== "select" || gesture)) {
            event.preventDefault();
            setTool("select");
        } else if (event.key === "Enter" || (event.key === " " && tool === "connect")) {
            const id = event.target.closest("[data-item]")?.dataset.item || api.getSelection();
            if (Object.hasOwn(SHAPES, tool)) {
                event.preventDefault();
                addNode(tool, [cx, cy]);
            } else if (tool === "connect") {
                event.preventDefault();
                connect(id);
            }
        } else if (event.key === " " && event.target === stage) {
            event.preventDefault();
            spacePan = true;
        }
    });
    window.addEventListener("keyup", event => { if (event.key === " ") spacePan = false; });
    window.addEventListener("blur", cancel);
    const observer = new ResizeObserver(paintView);
    observer.observe(stage);
    paintView();
    return {getView, fit, initialView, centerOn, zoomBy, setTool, getTool: () => tool,
        cancel, isBusy: () => !!gesture || !!from};
}
