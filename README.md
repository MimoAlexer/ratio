# Working Set

A desktop issue tracker for one developer per workspace, backed by a shared account. Home brings back your last checkpoint and critical security work. Notes, milestone dependencies and release charts point back to the underlying records. The application uses Rust, Axum, Maud server-rendered HTML, SQLx and PostgreSQL. Standard links and forms support the core workflow with JavaScript disabled.

## Desktop app

Install Working Set on Windows or Linux from the [private GitHub releases](https://github.com/MimoAlexer/working-set/releases). Connect both computers to the same HTTPS server and approve each device using the same account. Each installation has its own persistent session. The desktop host is Rust using Tao with WebView2 on Windows and WebKitGTK on Linux. The issue workspace, backend and data model are shared with the web service.

[Install and sign in](desktop/README.md) · [GitHub issues and project tracking](docs/github.md) · [Design direction and attribution](docs/design/direction.md) · [Original plans](docs/planning/product-plan.md)

![Working Set issue list in the Linux desktop app](docs/images/working-set-issues.png)

## Run locally

Install the Rust toolchain specified in `rust-toolchain.toml` and create a PostgreSQL database you control. PostgreSQL 18 is used in the integration suite. Set these variables in your shell, substituting your database credentials.

```bash
export DATABASE_URL='postgresql://workspace:YOUR_PASSWORD@127.0.0.1:5432/workspace'
export APP_ENV=development APP_BASE_URL=http://127.0.0.1:3000
export PORT=3000 BIND_ADDR=127.0.0.1 DEV_LOGIN_LINKS=1
cargo run --locked -- migrate
cargo run --locked -- serve
```

Open [the local workspace](http://127.0.0.1:3000). Enter an email address and use the development sign-in link, then confirm sign-in. The preview is restricted to direct loopback requests with matching origin and host. Create a project, add an issue with a next action, then return to Home. Production sign-in uses email delivery.

To explore the graphs with populated data, run `cargo run --locked -- seed-demo` in this **local development database**. Sign in as `demo@workspace.test`. Luma is a fictional project with native notes, manual critical work, milestones, issue history and published and draft releases. Re-running the seed preserves later edits. Your own account starts empty.

For a containerized setup, [the Railway and Compose runbook](ops/railway.md) covers the image, database roles, mail configuration, migrations, readiness, backup and recovery. The executable reads its environment directly. The [.env.example](.env.example) file lists variable names.

## Work in a project

- **Home and Issues.** Create a private project, add and edit issues, switch list/board, filter or search, save a checkpoint, link blockers, flag critical work, archive and undo. Manual findings and upstream GitHub alert status are separate.
- **Notes.** Write Markdown, follow wiki links and backlinks, turn a paragraph into an issue and open its source. The neighborhood graph and accessible relationship list use real workspace edges.
- **Roadmap and Health.** Set milestone dates, associate issues and releases, and draw explicit prerequisites. Selecting an item opens its source details. Multiple projects roll up on Max.
- **Releases and Insights.** Draft a scope, inspect what changed, select completed issues and publish an immutable shipment snapshot. The burnup, weekly scope accounting and issue cycle times use stored events. Imported baseline history and limited samples are labeled.
- **Import & export.** Preview a vault ZIP or a Linear issue CSV before committing changes. The native ZIP includes readable Markdown, stable IDs, manifests, issue history and eligible attachments. Contents and export access stay available after a downgrade. See [transfer and restoration](docs/transfers.md) and the Transfer page for the exact browser import size/record limits. Large archives can exceed them; keep an operator database backup for full disaster recovery.
- **Plans.** Free includes one active project/repository, Plus is $5/month with ten, and Max is $15/month with fifty. Plus adds saved views/templates and Max adds portfolio reporting. Core issues, notes, manual findings and export work without billing credentials. External billing uses Stripe-hosted checkout or its customer portal.

## Connect a provider

**Email.** Configure `RESEND_API_KEY` and a verified `MAIL_FROM`. Use the HTTPS app origin in `APP_BASE_URL`. Generate and store account recovery codes from Settings.

**GitHub.** Register a GitHub App requesting read access to metadata, pull requests and Dependabot alerts for selected repositories. Configure the app slug, OAuth client ID/secret, encrypted-token key, and webhook secret from the example environment file. Use `/integrations/github/callback` for the callback and `/integrations/github/webhook` for events. Open GitHub in the app to authorize access, bind an installation and map a repository to a project. Disabled or unavailable repositories show coverage status. Disconnect and reauthorize from the same page.

**Stripe.** Create recurring monthly USD prices of 5 and 15, set the corresponding price IDs, secret key and webhook signing secret. Point the webhook to `/billing/webhook`; set up the Stripe customer portal before testing hosted changes. Grant paid entitlements only after the provider confirms them. Keep test mode during qualification. Live credentials require explicitly enabling live mode. Provider credentials belong in the hosting platform’s secret store.

## Verify and operate

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
DATABASE_URL='postgresql://test_role:YOUR_PASSWORD@127.0.0.1:5432/tracker_test' \
  cargo test --locked -- --test-threads=3
```

Use a **dedicated disposable test database and role with CREATEDB**. SQLx integration tests create isolated databases and apply the real migrations. Provider tests use fixtures. The app serves `/healthz` for process health and `/readyz` for the database and expected migration checksums. See [operations](ops/railway.md) for deployment, provider setup and recovery.

The Rust server and HTML/SVG views are the working application. The separate design preview has its own fictional data. A hosted MCP service, in-app AI generation and continuous two-way Obsidian sync belong to the later roadmap.

For self-hosted monitoring, set a strong `OPS_METRICS_TOKEN` and scrape `/ops/metrics` with `Authorization: Bearer <token>`. Metrics include aggregate request latency and 5xx counts, pool occupancy, worker heartbeat and delays for GitHub sweeps, pending subscription cancellation and digest delivery. Keep this endpoint private. See [local qualification results](docs/verification.md) and the operations runbook before using real customer data.
