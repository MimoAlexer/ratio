// Writes only to the disposable architecture board created by the canvas browser test.
import {randomUUID} from "node:crypto";
import {setTimeout as delay} from "node:timers/promises";
import {testMapTools} from "./canvas-map-tools.mjs";

export async function testFeatures({page, base, boardId, assert}) {
  await testMapTools({page, base, boardId, assert});
  await testSourceCheckpoints({page, base, boardId, assert});
}

export async function testSourceCheckpoints({page, base, boardId, assert}) {
  const boardUrl = new URL("/api/canvas/" + boardId, base).href;
  const get = async (suffix = "") => {
    const response = await page.request.get(boardUrl + suffix);
    assert.equal(response.status(), 200, "Read " + response.url());
    return response.json();
  };
  const initial = await get();
  const node = initial.document.nodes[0];
  assert.ok(node, "The architecture template has a node");
  const records = await get("/sources?q=");
  const issue = records.find(record => record.kind === "issue");
  const note = records.find(record => record.kind === "note");
  assert.ok(issue && note, "The test project contains both source kinds");

  const dialog = page.locator("dialog[open]");
  async function closeDialog() {
    if (await dialog.count()) {
      await page.keyboard.press("Escape");
      await dialog.waitFor({state: "hidden"});
    }
  }
  async function openFeature(feature) {
    const action = page.locator('[data-feature="' + feature + '"]');
    if (!(await action.isVisible())) {
      await page.locator("#canvas-extras summary").filter({hasText: "Tools"}).click();
    }
    await action.click();
    await dialog.waitFor({state: "visible"});
  }
  async function selectNode() {
    await page.locator('[data-command="fit"]').click();
    await page.locator('g[data-type="node"][data-item="' + node.id + '"]').click();
  }
  function sourceOn(board) {
    return board.document.nodes.find(item => item.id === node.id)?.source ?? null;
  }
  async function awaitBoard(predicate, message) {
    const deadline = Date.now() + 12_000;
    do {
      const board = await get();
      if (predicate(board)) return board;
      await delay(120);
    } while (Date.now() < deadline);
    assert.fail(message);
  }
  async function link(record) {
    await selectNode();
    await openFeature("source");
    const search = dialog.getByLabel("Search project issues and notes", {exact: true});
    await search.fill(record.kind === "issue" ? record.label.split(/\s/)[0] : record.label);
    const result = dialog.getByRole("button", {name: "Link " + record.kind + ": " + record.label, exact: true});
    await result.click();
    await awaitBoard(board => sourceOn(board)?.id === record.id && sourceOn(board)?.kind === record.kind,
      "Source pin should autosave");
    await closeDialog();
  }
  async function unlink(record) {
    await selectNode();
    await openFeature("source");
    const linked = dialog.getByRole("link", {name: "Open linked " + record.kind, exact: true});
    const href = await linked.getAttribute("href");
    const sourceUrl = new URL(href, base);
    assert.equal(sourceUrl.pathname, "/" + record.kind + "s/" + record.id);
    const target = await page.request.get(sourceUrl.href, {maxRedirects: 0});
    assert.equal(target.status(), 200, "Linked record is readable using the same account");
    await dialog.getByRole("button", {name: "Unlink source", exact: true}).click();
    await awaitBoard(board => sourceOn(board) === null, "Unlink should autosave");
    await closeDialog();
  }

  await link(issue);
  await unlink(issue);
  await link(note);
  const snapshot = await get();
  await page.reload();
  await page.locator('[data-canvas-editor][data-ready="true"]').waitFor();
  await selectNode();
  await openFeature("source");
  const openNote = dialog.getByRole("link", {name: "Open linked note"});
  assert.equal(new URL(await openNote.getAttribute("href"), base).pathname,
    "/notes/" + note.id, "Pin remains after a full reload");
  await Promise.all([
    page.waitForURL(url => url.pathname === "/notes/" + note.id),
    openNote.click()
  ]);
  await page.goBack();
  await page.locator('[data-canvas-editor][data-ready="true"]').waitFor();
  await closeDialog();

  const checkpointName = "QA before changing the source " + randomUUID().slice(0, 8);
  await openFeature("checkpoints");
  await dialog.getByLabel("Checkpoint name", {exact: true}).fill(checkpointName);
  await dialog.getByRole("button", {name: "Save checkpoint", exact: true}).click();
  const restore = dialog.getByRole("button", {name: "Restore " + checkpointName, exact: true});
  await restore.waitFor();
  assert.ok((await get("/checkpoints")).some(row => row.name === checkpointName),
    "The named checkpoint is persisted");
  await closeDialog();
  await unlink(note);
  const edited = await get();
  await openFeature("checkpoints");
  await restore.click();
  const confirm = dialog.getByRole("button", {name: "Restore checkpoint", exact: true});
  await confirm.waitFor();
  assert.deepEqual((await get()).document, edited.document, "Previewing a restore does not change the board");
  await confirm.click();
  const restored = await awaitBoard(board => board.revision > edited.revision && sourceOn(board)?.id === note.id,
    "Restore creates a new revision containing the selected checkpoint");
  assert.deepEqual(restored.document, snapshot.document, "Restore recovers the complete saved diagram");
  await closeDialog();
  await openFeature("checkpoints");
  const remaining = await get("/checkpoints");
  assert.ok(remaining.some(row => row.automatic), "Restoring preserves the replaced drawing as a recovery point");
  const remove = dialog.getByRole("button", {name: "Delete " + checkpointName, exact: true});
  await remove.click();
  await dialog.getByRole("button", {name: "Cancel", exact: true}).click();
  await remove.click();
  await dialog.getByRole("button", {name: "Delete checkpoint", exact: true}).click();
  await dialog.getByText("Checkpoint deleted.", {exact: true}).waitFor();
  await remove.waitFor({state: "hidden"});
  assert.ok(!(await get("/checkpoints")).some(row => row.name === checkpointName),
    "Only the selected checkpoint was removed");
  await closeDialog();
  console.log("Canvas source pins and named checkpoint: passed");
}
