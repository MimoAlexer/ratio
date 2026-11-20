// Opt-in, read-only regression checks against the seeded local development workspace.
import assert from "node:assert/strict";
import {createRequire} from "node:module";
const require=createRequire(import.meta.url);
const {chromium}=require(process.env.PLAYWRIGHT_PATH || "playwright");
const base=new URL(process.env.RATIO_BASE_URL || "http://127.0.0.1:3000");
assert(["127.0.0.1","localhost","[::1]"].includes(base.hostname),"Use a loopback development server");
const browser=await chromium.launch({
  headless:true,
  ...(process.env.CHROME_BIN ? {executablePath:process.env.CHROME_BIN} : {})
});
try {
  const login=await browser.newContext();
  const page=await login.newPage();
  await page.goto(new URL("/login?demo=true",base).href);
  await page.locator('input[name="email"]').fill("demo@workspace.test");
  await page.locator('form[action="/login"] button[type=submit]').click();
  await page.getByRole("link",{name:"Open local sign-in link"}).click();
  await page.locator('form[action="/login/confirm"] button[type=submit]').click();
  assert.notEqual(new URL(page.url()).pathname,"/login");
  const storageState=await login.storageState();await login.close();
  for(const [width,height] of [[1440,900],[1280,800],[390,844],[844,390]]) {
    for(const javaScriptEnabled of [false,true]) {
      const context=await browser.newContext({viewport:{width,height},storageState,javaScriptEnabled,reducedMotion:"reduce",hasTouch:width<=844});
      const view=await context.newPage(),errors=[];
      view.on("pageerror",e=>errors.push(e.message));
      await view.goto(new URL("/issues?status=ready#search",base).href);
      const search=view.locator('#search input[name=q],input#search[name=q]').first();
      if(!await search.isVisible())await view.locator('a[href$="#search"]:visible').last().click();
      await search.fill("beta");
      await Promise.all([view.waitForURL(url=>url.searchParams.get("q")==="beta"),search.press("Enter")]);
      const found=new URL(view.url());
      assert.equal(found.pathname,"/issues");
      assert.equal(found.searchParams.get("status"),"ready");
      assert(!found.searchParams.has("save"),"Search Enter must not save a view");
      await view.getByText("Run the public beta acceptance checks",{exact:true}).first().waitFor({state:"visible"});
      assert(await view.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1));
      if(javaScriptEnabled) {
        await view.locator("[data-command-trigger]:visible").first().click();
        const input=view.getByRole("combobox").last();
        await input.fill("LU-32");
        await view.getByRole("option").filter({hasText:"Rebuild stale backlinks"}).first().waitFor({state:"visible"});
        await input.press("Escape");
        assert.equal(await view.locator("dialog[open]").count(),0);
      }
      assert.deepEqual(errors,[]); await context.close();
      console.log(width+"×"+height+" JS "+(javaScriptEnabled?"on":"off")+": passed");
    }
  }
} finally {await browser.close();}
