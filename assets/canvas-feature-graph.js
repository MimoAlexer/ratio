// Downstream follows the drawn arrows. The starting node is part of either trace.
export function traceDocument(doc, id, direction) {
    const nodes = new Set();
    const edges = new Set();
    const adjacent = new Map(doc.nodes.map(node => [node.id, []]));
    if (!adjacent.has(id) || !["upstream", "downstream"].includes(direction)) return {nodes, edges};

    for (const edge of doc.edges) {
        const from = direction === "upstream" ? edge.to : edge.from;
        const to = direction === "upstream" ? edge.from : edge.to;
        if (adjacent.has(from) && adjacent.has(to)) adjacent.get(from).push([to, edge.id]);
    }
    const pending = [id];
    nodes.add(id);
    while (pending.length) {
        for (const [next, edgeId] of adjacent.get(pending.pop())) {
            edges.add(edgeId);
            if (!nodes.has(next)) {
                nodes.add(next);
                pending.push(next);
            }
        }
    }
    return {nodes, edges};
}

// A block owns positions relative to its top-left corner. The final translation keeps
// the arrangement centred around the old drawing, including when it touches a world edge.
const COLUMN_GAP = 88;
const ROW_GAP = 40;
const GROUP_GAP = 112;
const WORLD_LIMIT = 16000;

function moveBlocks(blocks, gap, horizontal) {
    let offset = 0;
    const cross = Math.max(0, ...blocks.map(block => horizontal ? block.h : block.w));
    const positions = [];
    for (const block of blocks) {
        const x = horizontal ? offset : 0;
        const y = horizontal ? (cross - block.h) / 2 : offset;
        for (const item of block.positions) {
            positions.push({node: item.node, x: item.x + x, y: item.y + y});
        }
        offset += (horizontal ? block.w : block.h) + gap;
    }
    const length = Math.max(0, offset - gap);
    return {positions, w: horizontal ? length : cross, h: horizontal ? cross : length};
}

function shelf(blocks, width, gap) {
    const rows = [];
    let row = [];
    let used = 0;
    for (const block of blocks) {
        if (row.length && used + gap + block.w > width) {
            rows.push(moveBlocks(row, gap, true));
            used = 0;
            row = [];
        }
        used += (row.length ? gap : 0) + block.w;
        row.push(block);
    }
    if (row.length) rows.push(moveBlocks(row, gap, true));
    return moveBlocks(rows, gap, false);
}

function nodeBlock(node) {
    return {positions: [{node, x: 0, y: 0}], w: node.w, h: node.h};
}

function tidyComponent(component, outgoing, incoming) {
    // Condense each cycle into one rank first. It stays drawable and cannot keep
    // the topological queue waiting forever. Both traversals use model insertion order.
    const visited = new Set();
    const finished = [];
    function visit(node) {
        if (visited.has(node)) return;
        visited.add(node);
        for (const next of outgoing.get(node)) visit(next);
        finished.push(node);
    }
    for (const node of component) visit(node);

    const groupOf = new Map();
    const groups = [];
    function collect(node, group) {
        if (groupOf.has(node)) return;
        groupOf.set(node, group);
        for (const previous of incoming.get(node)) collect(previous, group);
    }
    for (const node of finished.reverse()) {
        if (groupOf.has(node)) continue;
        const group = {out: new Set(), count: 0, rank: 0};
        groups.push(group);
        collect(node, group);
    }

    for (const node of component) {
        const group = groupOf.get(node);
        for (const next of outgoing.get(node)) {
            const dependent = groupOf.get(next);
            if (dependent !== group && !group.out.has(dependent)) {
                group.out.add(dependent);
                dependent.count++;
            }
        }
    }
    const queue = groups.filter(group => group.count === 0);
    for (const group of queue) {
        for (const dependent of group.out) {
            dependent.rank = Math.max(dependent.rank, group.rank + 1);
            if (--dependent.count === 0) queue.push(dependent);
        }
    }

    const layers = [];
    let area = 0;
    for (const node of component) {
        const rank = groupOf.get(node).rank;
        (layers[rank] ??= []).push(nodeBlock(node));
        area += (node.w + COLUMN_GAP) * (node.h + ROW_GAP);
    }
    // Wrap long chains and wide fan-outs so either can still be explored at a useful zoom.
    const rowsPerColumn = Math.max(6, Math.ceil(Math.sqrt(component.length)));
    // A direct arrow that skips one or more ranks needs a clear route past the
    // intermediate shapes. Leave headroom in those ranks so a bypass does not
    // run through the middle of a simple chain.
    const crossing = Array(layers.length + 1).fill(0);
    for (const group of groups) {
        for (const dependent of group.out) {
            if (dependent.rank > group.rank + 1) {
                crossing[group.rank + 1]++;
                crossing[dependent.rank]--;
            }
        }
    }
    let spanningEdges = 0;
    const ranks = layers.map((layer, index) => {
        const columns = [];
        for (let i = 0; i < layer.length; i += rowsPerColumn) {
            columns.push(moveBlocks(layer.slice(i, i + rowsPerColumn), ROW_GAP, false));
        }
        const block = moveBlocks(columns, ROW_GAP, true);
        spanningEdges += crossing[index];
        if (spanningEdges) {
            const clearance = Math.max(...layer.map(item => item.h)) + ROW_GAP;
            block.h += clearance;
            for (const item of block.positions) item.y += clearance;
        }
        return block;
    });
    return shelf(ranks, Math.max(1400, Math.sqrt(area * 2)), COLUMN_GAP);
}

export function tidyDocument(doc) {
    if (doc.nodes.length < 2) return;
    const byId = new Map(doc.nodes.map(node => [node.id, node]));
    const outgoing = new Map(doc.nodes.map(node => [node, []]));
    const incoming = new Map(doc.nodes.map(node => [node, []]));
    for (const edge of doc.edges) {
        const from = byId.get(edge.from);
        const to = byId.get(edge.to);
        if (!from || !to) continue;
        outgoing.get(from).push(to);
        incoming.get(to).push(from);
    }

    const visited = new Set();
    const components = [];
    for (const start of doc.nodes) {
        if (visited.has(start)) continue;
        const component = [start];
        visited.add(start);
        for (const node of component) {
            for (const next of [...outgoing.get(node), ...incoming.get(node)]) {
                if (visited.has(next)) continue;
                visited.add(next);
                component.push(next);
            }
        }
        components.push(tidyComponent(component, outgoing, incoming));
    }

    const area = components.reduce((total, block) => total + (block.w + GROUP_GAP) * (block.h + GROUP_GAP), 0);
    let layout = shelf(components, Math.max(1200, Math.sqrt(area * 1.6)), GROUP_GAP);
    // At the model's limits a sparse graph can still waste space between ranks.
    // A regular packing of 250 maximum-size nodes fits comfortably inside the world.
    if (layout.w > WORLD_LIMIT * 2 - 2 || layout.h > WORLD_LIMIT * 2 - 2) {
        layout = shelf(layout.positions.map(({node}) => nodeBlock(node)), 11520, ROW_GAP);
    }
    const initialLeft = Math.min(...doc.nodes.map(node => node.x));
    const initialRight = Math.max(...doc.nodes.map(node => node.x + node.w));
    const initialTop = Math.min(...doc.nodes.map(node => node.y));
    const initialBottom = Math.max(...doc.nodes.map(node => node.y + node.h));
    function origin(low, high, size) {
        return Math.min(Math.floor(WORLD_LIMIT - size), Math.max(-WORLD_LIMIT, Math.round((low + high - size) / 2)));
    }
    const leftmost = Math.min(...layout.positions.map(item => item.x));
    const topmost = Math.min(...layout.positions.map(item => item.y));
    const rightmost = Math.max(...layout.positions.map(item => item.x + item.node.w));
    const bottommost = Math.max(...layout.positions.map(item => item.y + item.node.h));
    const left = origin(initialLeft, initialRight, rightmost - leftmost);
    const top = origin(initialTop, initialBottom, bottommost - topmost);
    for (const {node, x, y} of layout.positions) {
        node.x = left + Math.round(x - leftmost);
        node.y = top + Math.round(y - topmost);
    }
}
