// The desktop host serves this template locally and supplies window.ipc.
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {createRequire} from "node:module";

const require = createRequire(import.meta.url);
const {chromium} = require(process.env.PLAYWRIGHT_PATH || "playwright");
const assets = new URL("../../desktop/assets/", import.meta.url);
const template = await readFile(new URL("connect.html", assets), "utf8");
const mark = await readFile(new URL("mark.svg", assets), "utf8");
const browser = await chromium.launch({
  headless: true,
  ...(process.env.CHROME_BIN ? {executablePath: process.env.CHROME_BIN} : {})
});

try {
  const page = await browser.newPage();
  const saved = "https://workspace.example";
  const offline = "Ratio could not reach this server.";
  const values = {MARK: mark, ORIGIN: saved, MESSAGE: offline, SAVED_HIDDEN: "", DEMO_HIDDEN: "hidden", DEMO_ORIGIN: ""};
  const sent = [];
  const requests = [];
  page.on("request", request => requests.push(request.url()));
  await page.evaluate(() => {
    window.sent = [];
    window.ipc = {postMessage: body => window.sent.push(JSON.parse(body))};
  });
  await page.setContent(template.replace(/\{\{(\w+)\}\}/g, (_, key) => {
    assert(Object.hasOwn(values, key), "Unexpected launcher template key " + key);
    return values[key];
  }));
  const input = page.getByLabel("Workspace URL", {exact: true});
  const controls = page.getByRole("button", {name: "Desktop controls", exact: true});
  const retry = page.getByRole("button", {name: "Retry connection"});
  await page.waitForFunction(() => document.activeElement?.matches("#connection button[type=submit]"));
  assert.equal(await controls.getAttribute("aria-keyshortcuts"), "F10");
  await controls.click();
  sent.push({action: "controls"});
  assert.deepEqual(await page.evaluate(() => window.sent), sent);
  assert.equal(await input.inputValue(), saved);
  assert.equal(await page.getByRole("status").textContent(), offline);
  await retry.click();
  sent.push({action: "connect", origin: saved});
  await page.getByRole("button", {name: "Use another address"}).click();
  const replacement = "https://another.example";
  await input.fill(replacement);
  await page.evaluate(() => showInvalidAddress("Check this address."));
  await input.press("Shift+Tab");
  assert(await controls.evaluate(button => button === document.activeElement));
  await controls.press("Enter");
  sent.push({action: "controls"});
  assert.equal(await input.inputValue(), replacement);
  assert.equal(await input.getAttribute("aria-invalid"), "true");
  assert.equal(await page.getByRole("status").textContent(), "Check this address.");
  await input.press("Enter");
  sent.push({action: "connect", origin: replacement});
  assert.deepEqual(await page.evaluate(() => window.sent), sent);
  assert.deepEqual(requests, []);
  console.log("Launcher controls preserve input and feedback. Retry and Enter submit the expected IPC messages.");
} finally {
  await browser.close();
}
