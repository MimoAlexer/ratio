// Integration test. Use an isolated development PostgreSQL database and loopback server.
import assert from "node:assert/strict";
import {randomUUID} from "node:crypto";
import {createRequire} from "node:module";
import {setTimeout as delay} from "node:timers/promises";
import {testFeatures} from "./canvas-features.mjs";

const require = createRequire(import.meta.url);
const {chromium} = require(process.env.PLAYWRIGHT_PATH || "playwright");
assert(process.env.RATIO_BASE_URL && process.env.RATIO_CANVAS_QA === "1",
    "Set RATIO_CANVAS_QA=1 and RATIO_BASE_URL to an isolated local development server");
const base = new URL(process.env.RATIO_BASE_URL).origin;
assert(["127.0.0.1", "localhost", "[::1]"].includes(new URL(base).hostname));
const browser = await chromium.launch(process.env.CHROME_BIN ? {executablePath:process.env.CHROME_BIN} : {});
const errors = [];
const openPage = async context => {
    const page = await context.newPage();
    page.on("pageerror", error => errors.push(error.message));
    return page;
};

async function until(read, accept, reason) {
    const deadline = Date.now() + 15000;
    do {
        const value = await read();
        if (accept(value)) return value;
        await delay(120);
    } while (Date.now() < deadline);
    throw new Error(reason);
}
async function readBoard(page, id) {
    const response = await page.request.get(base + "/api/canvas/" + id);
    assert.equal(response.status(), 200);
    return response.json();
}
async function form(page, path, fields) {
    const csrf = await page.locator('input[name="csrf"]').first().inputValue();
    const response = await page.request.post(base + path, {form:{csrf,...fields}, maxRedirects:0});
    assert.equal(response.status(), 303, "Fixture write: " + path);
    return new URL(response.headers().location, base);
}
async function createProject(page) {
    const path = await form(page, "/projects", {name:"Canvas QA",key:"CQA"});
    const id = path.pathname.split("/").at(-1);
    await form(page, "/issues", {command_id:randomUUID(),project:id,title:"Retry import transaction"});
    await form(page, "/notes", {project:id,title:"Transfer protocol",path:"design/transfer.md",body:"Use an atomic transaction."});
    return id;
}
async function openBoard(page, id) {
    await page.goto(base + "/canvas/" + id);
    await page.locator('[data-canvas-editor][data-ready="true"] #canvas-stage[viewBox]').waitFor();
}
async function createBoard(page, project, template) {
    await page.goto(base + "/canvas?project=" + project);
    const form = page.locator('form[action="/canvas"][method="post"]');
    if (!await form.isVisible()) {
        await page.getByRole("link", {name:"New board",exact:true}).click();
        await form.waitFor();
    }
    await form.locator("input[name=title]").fill("Diagram QA · " + template);
    const choice = form.locator('input[name="template"][value="' + template + '"]');
    await form.locator("label").filter({has:page.locator('input[value="' + template + '"]')}).click();
    assert(await choice.isChecked(), "A pointer click on a template label must select it");
    await form.getByRole("button", {name:"Create board"}).click();
    await page.waitForURL(/\/canvas\/[a-f0-9-]{36}$/);
    await page.locator('[data-canvas-editor][data-ready="true"] #canvas-stage[viewBox]').waitFor();
    return new URL(page.url()).pathname.split("/").at(-1);
}

const command = (page, name) => page.locator('button[data-command="' + name + '"]');
async function activate(page, tool) {
    const button = page.locator('#canvas-tools button[data-tool="' + tool + '"]');
    if (!await button.isVisible()) await page.locator(".canvas-shapes > summary").click();
    await button.click();
}
async function point(page, fx, fy) {
    const rect = await page.locator("#canvas-stage").boundingBox();
    return [rect.x + rect.width*fx, rect.y + rect.height*fy];
}
async function worldPoint(page, x, y) {
    return page.locator("#canvas-stage").evaluate((svg,[x,y]) => {
        const p = new DOMPoint(x,y).matrixTransform(svg.getScreenCTM());
        return [p.x,p.y];
    },[x,y]);
}
async function draw(page, id) {
    const wait = (fn, why) => until(() => readBoard(page,id), b => fn(b.document), why);
    const label = page.locator('#canvas-inspector [name=label]');
    async function add(kind, text, fx, fy) {
        await activate(page, kind);
        await page.mouse.click(...await point(page,fx,fy));
        await label.fill(text);
        await label.press("Enter");
        return (await wait(d => d.nodes.some(n => n.label===text), "Node label failed to persist"))
            .document.nodes.find(n=>n.label===text);
    }
    const process = await add("process","Parse manifest",.32,.34);
    const decision = await add("decision","Valid manifest?",.52,.63);
    assert.equal(process.kind,"process");
    assert.equal(decision.kind,"decision");
    const snap = command(page,"snap");
    assert.equal(await snap.getAttribute("aria-pressed"),"true");
    await page.getByRole("button",{name:"Close inspector"}).click();
    await activate(page,"select");
    const start = await worldPoint(page,process.x+process.w/2,process.y+process.h/2);
    await page.mouse.move(...start);
    await page.mouse.down();
    await page.mouse.move(start[0]+37,start[1]+25,{steps:8});
    await page.mouse.up();
    let doc = (await wait(d=>d.nodes.find(n=>n.id===process.id).x!==process.x,"Drag failed")).document;
    const moved=doc.nodes.find(n=>n.id===process.id);
    assert.equal((moved.x-process.x)%16,0);
    assert.equal((moved.y-process.y)%16,0);
    await snap.click();
    assert.equal(await snap.getAttribute("aria-pressed"),"false");
    await activate(page,"connect");
    for (const n of [moved,decision]) await page.mouse.click(...await worldPoint(page,n.x+n.w/2,n.y+n.h/2));
    doc=(await wait(d=>d.edges.some(e=>e.from===process.id && e.to===decision.id),"Directional connector failed")).document;
    await activate(page,"pen");
    const a = await point(page,.22,.68);
    await page.mouse.move(...a);
    await page.mouse.down();
    await page.mouse.move(a[0]+35,a[1]-24,{steps:7});
    await page.mouse.move(a[0]+70,a[1]+8,{steps:7});
    await page.mouse.up();
    await wait(d=>d.strokes.length===1 && d.strokes[0].points.length>=3,"Pen stroke failed");
    await command(page,"undo").click();
    await wait(d=>d.strokes.length===0 && d.edges.length===1,"Undo removed more than a stroke");
    await command(page,"redo").click();
    await wait(d=>d.strokes.length===1,"Redo failed");
    const stage=page.locator("#canvas-stage");
    const before=await stage.getAttribute("viewBox");
    await command(page,"zoom-in").click();
    assert.notEqual(await stage.getAttribute("viewBox"),before);
    await command(page,"fit").click();
    await page.reload();
    await page.locator('#canvas-stage [data-item="' + process.id + '"]').waitFor();
    assert.equal(await snap.getAttribute("aria-pressed"),"false","Snap preference survived reopen");
    await stage.press("]");
    await stage.press("ArrowRight");
    await wait(d => d.nodes.find(n => n.id === process.id).x > moved.x, "Keyboard move failed");
    await stage.press("Delete");
    await wait(d => d.nodes.length === 1 && d.edges.length === 0, "Deleting a node must remove its connectors");
    await stage.press("Control+z");
    await wait(d => d.nodes.length === 2 && d.edges.length === 1, "Undo must restore the node and its connectors");
    const saved=await readBoard(page,id);
    assert.equal(saved.document.nodes.length,2);
    assert.equal(saved.document.edges.length,1);
    assert.equal(saved.document.strokes.length,1);
}

async function conflict(page, other, id) {
    await openBoard(page,id);
    await openBoard(other,id);
    const api=base+"/api/canvas/"+id;
    let release, intercepted = false;
    const proceed=new Promise(resolve=>{release=resolve;});
    const handler=async route=>{
        if (route.request().method()!=="POST") return route.continue();
        intercepted = true; await proceed; await route.continue();
    };
    await page.route(api,handler);
    try {
        await page.locator("#canvas-title").fill("My local draft");
        await page.locator("#canvas-title").press("Tab");
        await until(() => intercepted, Boolean, "Local edit did not start saving");
        await other.locator("#canvas-title").fill("Other device revision");
        await other.locator("#canvas-title").press("Tab");
        await until(()=>readBoard(other,id),b=>b.title==="Other device revision","Other device save failed");
    } finally { release(); }
    const recovery=page.getByRole("button",{name:"Use server version"});
    await recovery.waitFor();
    assert.equal(await page.locator("#canvas-title").inputValue(),"My local draft");
    assert.equal((await readBoard(other,id)).title,"Other device revision");
    await page.unroute(api,handler);
    page.once("dialog",d=>d.accept());
    await recovery.click();
    await until(()=>page.locator("#canvas-title").inputValue(),v=>v==="Other device revision","Conflict recovery failed");
}
try {
    const context=await browser.newContext({viewport:{width:1440,height:900},reducedMotion:"reduce"});
    const page=await openPage(context);
    await page.goto(base+"/login");
    await page.locator('input[name="email"]').fill("canvas-qa-"+randomUUID()+"@workspace.test");
    await page.locator('form[action="/login"] button[type=submit]').click();
    await page.getByRole("link",{name:"Open local sign-in link"}).click();
    await page.locator('form[action="/login/confirm"] button[type=submit]').click();
    await page.waitForURL(base+"/");
    const project=await createProject(page);
    const blank=await createBoard(page,project,"blank");
    await draw(page,blank);
    const architecture=await createBoard(page,project,"architecture");
    await testFeatures({page,base,boardId:architecture,assert});
    for (const template of ["release","decision"]) {
        const id=await createBoard(page,project,template), {document}=await readBoard(page,id);
        assert(document.nodes.length>=3 && document.edges.length>=2,template+" template is populated");
    }
    const peer=await browser.newContext({storageState:await context.storageState(),viewport:{width:1440,height:900}});
    const other=await openPage(peer);
    await conflict(page,other,blank);
    await page.goto(base + "/canvas?project=" + project);
    const boardActions = page.locator(".canvas-board-actions").first();
    await boardActions.locator("summary").click();
    const confirmation = boardActions.locator("input[name=confirmation]");
    // Activating its label must move focus to the field without closing the disclosure.
    await boardActions.locator("label").click({position:{x:8,y:7}});
    assert(await confirmation.evaluate(input => input === document.activeElement));
    await boardActions.locator("summary").click();
    await form(page,"/projects/"+project+"/archive",{});
    await openBoard(page,blank);
    assert.equal((await readBoard(page,blank)).read_only,true);
    assert(await page.locator("#canvas-title").evaluate(e=>e.readOnly));
    assert(!await page.locator('#canvas-tools [data-tool="pen"]').isVisible());
    assert.deepEqual(errors,[]);
    console.log("Canvas drawing, templates, source pins, snap, layout, trace, search, minimap, history, checkpoints, SVG export, conflict recovery and archived viewing passed.");
} finally {await browser.close();}
