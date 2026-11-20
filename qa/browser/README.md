# Browser regression checks

Run the Ratio development service on loopback with `DEV_LOGIN_LINKS=1` and an explicitly seeded Luma demo. The workbench checks sign in and read sample records. From this directory:

```sh
npm ci
npx playwright install chromium
RATIO_BASE_URL=http://127.0.0.1:3000 npm test
```

The four viewport sizes cover desktop, laptop, phone and short landscape. The test submits a filtered query with Enter in both JS modes, verifies that the issue-search GET is used, checks the matched record, and exercises the enhanced command dialog. Set `CHROME_BIN` to use a system Chromium executable.

With Chromium installed, run `npm run test:launcher` to check the bundled desktop connection form offline. It verifies the keyboard and pointer paths to Desktop controls, preserves validation and recovery messages, and checks the messages sent on Retry and Enter.


## Planning canvas

Run the canvas checks against a **disposable development database** with local sign-in links enabled. The test creates a new fictional account, project, issue, note and planning boards. It exercises drawing, save and reload, simultaneous edits from two browser sessions, and archive permissions.

```sh
RATIO_CANVAS_QA=1 RATIO_BASE_URL=http://127.0.0.1:3000 npm run test:canvas
```
