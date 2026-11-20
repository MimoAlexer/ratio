import {tidyDocument, traceDocument} from './canvas-feature-graph.js';
import {mountMinimap, mountSearch} from './canvas-feature-navigation.js';
import {mountSources, mountCheckpoints} from './canvas-feature-data.js';

const icons = {
    tools: 'M3 6h11m4 0h3M3 18h3m4 0h11M14 3v6h4V3zM6 15v6h4v-6z',
    find: 'M15.5 9.5a6 6 0 1 1-12 0 6 6 0 0 1 12 0ZM14 14l6.5 6.5',
    source: 'm10 13 4-4m-5.5 6.5-1 1a3.5 3.5 0 0 1-5-5L6 8a3.5 3.5 0 0 1 5 0m2 6a3.5 3.5 0 0 0 5 0l3.5-3.5a3.5 3.5 0 0 0-5-5l-1 1',
    tidy: 'M3 3h7v6H3zM14 3h7v6h-7zM8.5 15h7v6h-7zM6.5 9v3h11V9M12 12v3',
    upstream: 'M12 21v-8c0-3-2-5-5-5H3m0 0 3-3M3 8l3 3m6 3c0-4 2-6 5-6h4m0 0-3-3m3 3-3 3',
    downstream: 'M3 4c0 6 2 9 7 9h4c4 0 6 3 6 6m-3-3 3 3 3-3M20 4c0 4-2 6-6 6M10 13c-4 0-6 3-6 6m-3-3 3 3 3-3',
    checkpoints: 'M4 8a8 8 0 1 1 0 8M4 3v5h5M12 7v5l4 3',
    export: 'M12 3v12m-5-5 5 5 5-5M4 16v4h16v-4',
    close: 'm6 6 12 12M18 6 6 18',
    chevron: 'm7 10 5 5 5-5',
};

function icon(name) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('focusable', 'false');
    svg.classList.add('canvas-feature-icon');
    const path = document.createElementNS(svg.namespaceURI, 'path');
    path.setAttribute('d', icons[name]);
    svg.append(path);
    return svg;
}

function button(label, name) {
    const control = document.createElement('button');
    control.type = 'button';
    control.append(icon(name), document.createTextNode(label));
    return control;
}

export function mountFeatures(api) {
    const {root} = api;
    const host = root.querySelector('#canvas-extras');
    const stage = root.querySelector('#canvas-stage');
    const tools = document.createElement('details');
    tools.className = 'canvas-feature-tools';
    const summary = document.createElement('summary');
    summary.append(icon('tools'), document.createTextNode('Tools'), icon('chevron'));
    tools.append(summary);

    const list = document.createElement('div');
    list.className = 'canvas-feature-menu';
    tools.append(list);
    const controls = new Map();
    function action(name, label) {
        const control = button(label, name);
        control.dataset.feature = name;
        list.append(control);
        controls.set(name, control);
    }
    function divider() {
        list.append(document.createElement('hr'));
    }
    action('find', 'Find on canvas');
    action('source', 'Link issue or note');
    divider();
    action('tidy', 'Tidy layout');
    action('upstream', 'Trace upstream');
    action('downstream', 'Trace downstream');
    divider();
    action('checkpoints', 'Checkpoints');
    action('export', 'Export SVG');

    const clear = button('Clear trace', 'close');
    clear.className = 'canvas-clear-trace';
    clear.dataset.feature = 'clear-trace';
    clear.hidden = true;
    const dialog = document.createElement('dialog');
    dialog.className = 'canvas-feature-dialog';
    dialog.setAttribute('aria-labelledby', 'canvas-feature-heading');
    const header = document.createElement('header');
    const heading = document.createElement('h2');
    heading.id = 'canvas-feature-heading';
    const dismiss = button('', 'close');
    dismiss.className = 'canvas-feature-dismiss';
    dismiss.setAttribute('aria-label', 'Close');
    header.append(heading, dismiss);
    const panel = document.createElement('div');
    panel.className = 'canvas-feature-panel';
    dialog.append(header, panel);
    host.append(clear, tools, dialog);
    mountMinimap(api, root.querySelector('#canvas-minimap'));

    let cleanupPanel;
    let focusOnClose;
    function closePanel(result) {
        if (result === 'selection') focusOnClose = stage;
        if (dialog.open) dialog.close();
    }
    function openPanel(title, mount) {
        if (dialog.open) return;
        heading.textContent = title;
        panel.replaceChildren();
        focusOnClose = summary;
        dialog.showModal();
        cleanupPanel = mount(api, panel, closePanel);
    }
    dismiss.addEventListener('click', closePanel);
    dialog.addEventListener('close', () => {
        cleanupPanel?.();
        cleanupPanel = undefined;
        panel.replaceChildren();
        (focusOnClose?.isConnected ? focusOnClose : summary).focus({preventScroll: true});
    });
    dialog.addEventListener('click', event => {
        if (event.target !== dialog) return;
        const bounds = dialog.getBoundingClientRect();
        if (event.clientX < bounds.left || event.clientX > bounds.right
            || event.clientY < bounds.top || event.clientY > bounds.bottom) closePanel();
    });

    let trace = null;
    let tracedDoc;
    let exporting = false;
    let previousState;
    function clearTrace() {
        trace = null;
        tracedDoc = undefined;
        api.setTrace(null);
        clear.hidden = true;
        controls.get('upstream').setAttribute('aria-pressed', 'false');
        controls.get('downstream').setAttribute('aria-pressed', 'false');
    }
    function renderTrace(doc, announce) {
        const route = traceDocument(doc, trace.id, trace.direction);
        tracedDoc = doc;
        api.setTrace(route);
        clear.hidden = false;
        clear.title = 'Clear ' + trace.direction + ' trace';
        for (const direction of ['upstream', 'downstream']) {
            controls.get(direction).setAttribute('aria-pressed', String(trace.direction === direction));
        }
        if (announce) {
            const count = route.nodes.size - 1;
            api.notify(count + ' ' + trace.direction + (count === 1 ? ' node' : ' nodes'));
        }
    }
    function update() {
        const board = api.getBoard();
        const doc = board.document;
        const selected = api.getSelection();
        // Viewport changes also notify subscribers; controls only depend on the diagram and selection.
        if (previousState && previousState.doc === doc && previousState.selected === selected
            && previousState.readOnly === board.read_only && previousState.exporting === exporting) return;
        previousState = {doc, selected, readOnly: board.read_only, exporting};
        const isNode = doc.nodes.some(node => node.id === selected);
        const source = controls.get('source');
        source.disabled = !isNode;
        source.title = isNode ? '' : 'Select a node first';
        controls.get('tidy').disabled = board.read_only || doc.nodes.length < 2;
        const upward = controls.get('upstream');
        const downward = controls.get('downstream');
        upward.disabled = !isNode || !doc.edges.some(edge => edge.to === selected);
        downward.disabled = !isNode || !doc.edges.some(edge => edge.from === selected);
        upward.title = upward.disabled ? 'Select a node with incoming connections' : '';
        downward.title = downward.disabled ? 'Select a node with outgoing connections' : '';
        controls.get('export').disabled = exporting;
        if (trace && !doc.nodes.some(node => node.id === trace.id)) clearTrace();
        else if (trace && doc !== tracedDoc) renderTrace(doc, false);
    }
    clear.addEventListener('click', () => {
        clearTrace();
        stage.focus({preventScroll: true});
    });
    summary.addEventListener('click', update);
    list.addEventListener('click', async event => {
        const control = event.target.closest('button[data-feature]');
        if (!control || control.disabled) return;
        tools.open = false;
        summary.focus({preventScroll: true});
        const command = control.dataset.feature;
        switch (command) {
        case 'find':
            openPanel('Find on canvas', mountSearch);
            break;
        case 'source':
            openPanel('Link issue or note', mountSources);
            break;
        case 'checkpoints':
            openPanel('Checkpoints', mountCheckpoints);
            break;
        case 'tidy':
            api.change(tidyDocument);
            api.fit();
            api.notify('Layout arranged');
            break;
        case 'upstream':
        case 'downstream': {
            const selected = api.getSelection();
            if (trace?.id === selected && trace.direction === command) {
                clearTrace();
            } else {
                trace = {id: selected, direction: command};
                renderTrace(api.getBoard().document, true);
            }
            break;
        }
        case 'export':
            exporting = true;
            update();
            try {
                if (!await api.flush()) {
                    api.notify('Save your changes before exporting');
                    break;
                }
                const link = document.createElement('a');
                link.href = '/canvas/' + encodeURIComponent(api.getBoard().id) + '/export.svg';
                link.download = '';
                host.append(link);
                link.click();
                link.remove();
            } catch {
                api.notify('Could not start the export');
            } finally {
                exporting = false;
                update();
            }
            break;
        }
    });
    tools.addEventListener('keydown', event => {
        if (event.key === 'Escape' && tools.open) {
            event.preventDefault();
            event.stopPropagation();
            tools.open = false;
            summary.focus();
            return;
        }
        if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
        if (!tools.open && event.target !== summary) return;
        update();
        const items = [...list.querySelectorAll('button:not(:disabled)')];
        if (!items.length) return;
        event.preventDefault();
        event.stopPropagation();
        tools.open = true;
        const index = items.indexOf(document.activeElement);
        let next = 0;
        if (event.key === 'End' || (event.key === 'ArrowUp' && index < 0)) next = items.length - 1;
        else if (event.key === 'ArrowDown') next = (index + 1) % items.length;
        else if (event.key === 'ArrowUp') next = (index + items.length - 1) % items.length;
        items[next].focus();
    });
    document.addEventListener('pointerdown', event => {
        if (tools.open && !tools.contains(event.target)) tools.open = false;
    });
    tools.addEventListener('focusout', event => {
        if (event.relatedTarget && !tools.contains(event.relatedTarget)) tools.open = false;
    });

    api.onChange(update);
    update();
}
