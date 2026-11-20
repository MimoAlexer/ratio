// Run on the disposable architecture board prepared by canvas.mjs.
export async function testMapTools({page, base, boardId, assert}) {
  const endpoint = new URL('/api/canvas/' + boardId, base).href;
  const stage = page.locator('#canvas-stage');
  const item = id => stage.locator('[data-item="' + id + '"]');
  const command = name => page.locator('[data-command="' + name + '"]');
  const feature = name => page.locator('[data-feature="' + name + '"]');
  const toolsSummary = page.locator('#canvas-extras summary').filter({hasText: 'Tools'});

  async function runTool(name) {
    if (!await feature(name).isVisible()) await toolsSummary.click();
    await feature(name).click();
  }

  async function savedBoard() {
    const response = await page.request.get(endpoint);
    assert.equal(response.status(), 200);
    return response.json();
  }

  const isSave = response => response.url() === endpoint && response.request().method() === 'POST';
  async function saveAction(action) {
    const [response] = await Promise.all([page.waitForResponse(isSave), action()]);
    assert.equal(response.status(), 200);
    return (await savedBoard()).document;
  }

  function tracedIds(doc, start, upstream) {
    const nodes = new Set([start]);
    const edges = new Set();
    const queue = [start];
    while (queue.length) {
      const id = queue.shift();
      for (const edge of doc.edges) {
        const from = upstream ? edge.to : edge.from;
        if (from !== id) continue;
        edges.add(edge.id);
        const to = upstream ? edge.from : edge.to;
        if (nodes.has(to)) continue;
        nodes.add(to);
        queue.push(to);
      }
    }
    return {node: [...nodes].sort(), edge: [...edges].sort()};
  }

  const viewport = () => stage.evaluate(svg => {
    const {x, y, width, height} = svg.viewBox.baseVal;
    return {x, y, width, height};
  });

  const before = (await savedBoard()).document;
  assert(before.edges.length >= 2, 'Use a connected architecture board for map interactions');
  const startingEdge = before.edges[0];
  // Complete a directed cycle and verify traversal terminates in each direction.
  await command('fit').click();
  let graph = before;
  if (!before.edges.some(edge => edge.from === startingEdge.to && edge.to === startingEdge.from)) {
    graph = await saveAction(async () => {
      await page.locator('button[data-tool="connect"]').click();
      await item(startingEdge.to).click();
      await item(startingEdge.from).click();
    });
    assert(graph.edges.some(edge => edge.from === startingEdge.to && edge.to === startingEdge.from));
  }
  await page.locator('button[data-tool="select"]').click();

  const reference = graph.nodes.find(node => node.id === startingEdge.to);
  await runTool('find');
  const dialog = page.getByRole('dialog', {name: 'Find on canvas'});
  const input = dialog.locator('input');
  await input.fill('absent-1f31615f');
  assert.equal(await dialog.getByRole('option').count(), 0);
  await input.fill(reference.label);
  await dialog.getByRole('option').filter({hasText: reference.label}).first().waitFor({state: 'visible'});
  await input.press('Enter');
  await dialog.waitFor({state: 'hidden'});
  await stage.locator('[data-item="' + reference.id + '"][data-selected="true"]').waitFor();

  const foundView = await viewport();
  assert(Math.abs(foundView.x + foundView.width / 2 - (reference.x + reference.w / 2)) < 1 &&
    Math.abs(foundView.y + foundView.height / 2 - (reference.y + reference.h / 2)) < 1,
    'Find must center the chosen node');

  for (const direction of ['downstream', 'upstream']) {
    await runTool(direction);
    await stage.locator('[data-type=node][data-traced=true]').first().waitFor();
    const expected = tracedIds(graph, reference.id, direction === 'upstream');
    for (const type of ['node', 'edge']) {
      const ids = await stage.locator('[data-type=' + type + '][data-traced=true]').evaluateAll(items =>
        items.map(element => element.getAttribute('data-item')).sort());
      assert.deepEqual(ids, expected[type], direction + ' reachable ' + type + 's');
    }
    await runTool('clear-trace');
    assert.equal(await stage.locator('[data-traced]').count(), 0);
  }

  const sorted = await saveAction(() => runTool('tidy'));
  const positions = doc => doc.nodes.map(({id, x, y}) => [id, x, y]).sort();
  assert.notDeepEqual(positions(sorted), positions(graph), 'Tidy must move the architecture template');
  assert.deepEqual(sorted.edges, graph.edges, 'Keep connector topology, labels and identities');
  for (const [index, node] of sorted.nodes.entries()) {
    for (const sibling of sorted.nodes.slice(index + 1)) {
      assert(node.x + node.w <= sibling.x || sibling.x + sibling.w <= node.x ||
        node.y + node.h <= sibling.y || sibling.y + sibling.h <= node.y, 'Tidy layout must separate nodes');
    }
  }

  await command('zoom-in').click();
  await command('zoom-in').click();
  const old = await viewport();
  const map = page.getByRole('group', {name: 'Map overview'});
  const toggle = page.locator('#canvas-minimap .canvas-map-toggle');
  const wasCollapsed = !await map.isVisible();
  if (wasCollapsed) await toggle.click();
  const size = await map.boundingBox();
  assert(size && size.width > 10 && size.height > 10, 'Navigable minimap must be displayed');
  await map.click({position: {x: size.width * 0.72, y: size.height * 0.27}});
  const current = await viewport();
  assert(Math.abs(current.x - old.x) > 1 || Math.abs(current.y - old.y) > 1, 'Minimap must pan the canvas');
  assert.equal(current.width, old.width, 'Minimap panning preserves zoom');
  await map.press('ArrowRight');
  assert((await viewport()).x > current.x, 'Arrow key pans the map');
  if (wasCollapsed) await toggle.click();
  await command('fit').click();

  const [download] = await Promise.all([page.waitForEvent('download'), runTool('export')]);
  assert.match(download.suggestedFilename(), /\.svg$/i);
  assert.equal(new URL(download.url()).pathname, '/canvas/' + boardId + '/export.svg');
  const stream = await download.createReadStream();
  const chunks = [];
  for await (const chunk of stream) chunks.push(chunk);
  const bytes = Buffer.concat(chunks).toString('utf8');
  const inspected = await page.evaluate(svgText => {
    const doc = new DOMParser().parseFromString(svgText, 'image/svg+xml');
    const root = doc.documentElement;
    const active = [...doc.querySelectorAll('*')].some(element =>
      ['script', 'foreignobject', 'iframe', 'image'].includes(element.localName.toLowerCase()) ||
      [...element.attributes].some(attr => /^on/i.test(attr.name) ||
        (attr.localName === 'href' && !attr.value.startsWith('#'))));
    const [x, y, width, height] = (root.getAttribute('viewBox') || '').trim().split(/[ ,]+/).map(Number);
    return {tag: root.localName, namespace: root.namespaceURI, active,
      validBounds: Number.isFinite(x) && Number.isFinite(y) && width > 0 && height > 0,
      text: doc.documentElement.textContent, edges: doc.querySelectorAll('.connector').length,
      strokes: doc.querySelectorAll('.ink').length};
  }, bytes);
  assert.equal(inspected.tag, 'svg');
  assert.equal(inspected.namespace, 'http://www.w3.org/2000/svg');
  assert(inspected.validBounds);
  assert.equal(inspected.active, false, 'SVG must work without external code or files');
  assert.equal(inspected.edges, sorted.edges.length, 'SVG includes every connector');
  assert.equal(inspected.strokes, sorted.strokes.length, 'SVG includes every ink stroke');
  for (const node of sorted.nodes) assert(inspected.text.includes(node.label), 'Missing exported node ' + node.label);
  for (const edge of sorted.edges) if (edge.label) assert(inspected.text.includes(edge.label), 'Missing exported connector ' + edge.label);
  if (await page.locator('#canvas-extras details[open]').count()) await toolsSummary.click();
}
