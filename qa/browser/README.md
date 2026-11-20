# Browser regression checks

Run the Ratio development service on loopback with `DEV_LOGIN_LINKS=1` and an explicitly seeded Luma demo. These tests only sign in and read sample records. From this directory:

```sh
npm ci
npx playwright install chromium
RATIO_BASE_URL=http://127.0.0.1:3000 npm test
```

The four viewport sizes cover desktop, laptop, phone and short landscape. The test submits a filtered query with Enter in both JS modes, verifies that the issue-search GET is used, checks the matched record, and exercises the enhanced command dialog. Set `CHROME_BIN` to use a system Chromium executable.
