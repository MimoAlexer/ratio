# Run Workspace on Railway

The web service stores accounts, work items, notes, release history and private image bytes in PostgreSQL. The same database holds their relationships and checksums. A replacement web container can start with an empty filesystem. ZIP exports use private scratch files. Provide a writable, file-backed `/tmp` (or `TMPDIR`) with enough ephemeral disk for two concurrent exports, including their manifests and histories. Monitor free scratch space during a large export.

## Verified local image

The November 20, 2026 Linux/amd64 build used Docker 29, the checked-in Dockerfile and Rust 1.98.1. The local OCI image index digest was `sha256:6a4048b740eeb04f734f8f335cb0d1d8d6602ac27d89bcfe4db8b987e763d2a7`, with source-input checksum `edf4e849bed89342b3fb8306cbc8c6b15ed9a68538e1e78900b0812d9f680fd6`. The server ran under UID/GID 65532. The production-mode smoke checked schema rejection, 19 migrations, a non-owner database login, the injected port, cookie attributes, private-route redirects and storage accounting. In a loopback-only development instance of the same image, sign-in, 11 private pages, project creation and a full Compose restart passed. The project and session survived the restart. This was a local build. Hosted deployment, live mail and off-account recovery still need staging qualification.

## Run locally with Compose

From the repository root, use Docker Compose v2 or newer. Save two independently generated hexadecimal passwords in a local secret manager. Export `POSTGRES_PASSWORD` for the migration owner and `WORKSPACE_DB_PASSWORD` for the non-owner web role. Reuse the same pair when restarting an existing volume. Run `openssl rand -hex 32` separately for each value. Set `RESEND_API_KEY` and `MAIL_FROM` in your environment to receive sign-in email through Compose. For email-free development, use the native loopback launch in the repository README. Docker's bridge gives the request a non-loopback peer address.

```sh
docker compose up --build -d
curl --retry 30 --retry-connrefused --retry-delay 1 --fail http://127.0.0.1:3000/readyz
```

Open `http://127.0.0.1:3000`. Set `WORKSPACE_PORT` to publish a different loopback port. `docker compose down` stops the services and retains the named database volume. The one-shot `migrate` service applies migrations and grants. `web` receives only the `workspace_app` connection. Postgres has no published host port. To adopt or rotate the role for an existing volume after exporting its new password, run `docker compose up -d db`, `docker compose exec db /docker-entrypoint-initdb.d/010-runtime.sh`, then `docker compose up --build -d`. `docker compose exec db psql -U workspace -d workspace` opens a local database console.

## Prepare staging

1. Create an empty Railway project and a separate empty `staging` environment. Give staging synthetic data, a dedicated sending identity and test provider credentials. Choose the actual service region before provisioning. The checked-in example places web and Postgres together in `us-west2` (California). Change the `region` constant in `.railway/railway.ts` if your users or residency requirements call for another location. [Railway regions](https://docs.railway.com/deployments/regions)
2. Decide the final HTTPS origin for the staging web service. Set the environment's shared variables `APP_BASE_URL` to that origin, `MAIL_FROM` to a verified sender and `RESEND_API_KEY` to its sending key. Generate a separate random hexadecimal database password for `workspace_app`, and store its complete private connection string under the shared secret `WORKSPACE_RUNTIME_DATABASE_URL`. Use the intended database name and private hostname for `postgres`. Verify both against the provisioned service before connecting the app source. Keep keys in Railway's secret controls. The authoring file contains reference names only. Configure a custom domain or generate a Railway web domain in service Networking and confirm the configured origin matches exactly before anyone requests an email link. [Railway variables](https://docs.railway.com/variables) · [Public networking](https://docs.railway.com/networking/public-networking)
3. Use an official Node.js 22.13+ distribution with TypeScript stripping, npm and Railway CLI 5.75.0. Run the following from the repository root. The first two commands type-check the file and resolve both environment definitions locally. The plan command also needs a linked project and reads the live environment. [Railway IaC workflow](https://docs.railway.com/infrastructure-as-code) · [TypeScript reference](https://docs.railway.com/infrastructure-as-code/reference)

   ```sh
   npm ci --prefix .railway --ignore-scripts
   npm run --prefix .railway check
   railway login
   railway link
   railway config plan
   ```

4. Confirm the CLI identifies the intended project and `staging`. Review the `workspace` partial, the managed PostgreSQL service and its persistent volume, and the web service. Review every removal and region change. Run `railway config apply` after accepting the plan. No automated workflow applies infrastructure. Keep Postgres private. If the template created public TCP networking, remove that proxy in the Postgres Networking panel. Configure public HTTPS on `web` only. [Railway private networking](https://docs.railway.com/networking/private-networking/how-it-works)
5. Bootstrap the non-owner login on the new database from a private administrative session with PostgreSQL 18 client tools. Set `PGHOST`, `PGPORT`, `PGUSER`, `PGDATABASE`, `PGPASSFILE` and the separately held `WORKSPACE_DB_PASSWORD`, then run `ops/init-runtime.sh`. Verify `WORKSPACE_RUNTIME_DATABASE_URL` uses `workspace_app`, that password, and the actual private database host and database name. A successful pre-deploy uses `MIGRATION_DATABASE_URL` and `APP_GRANT_RUNTIME=1` to apply the schema and grant the runtime role data access. Its grants allow reading migration versions, inserting erasure markers and updating work items. Only the recovery operator can clear an identity hold. The web start command removes the migration URL from the server process environment. The two Railway phases still share service-level secret settings, so restrict access to the service dashboard and deploy configuration.
6. Upload the reviewed repository revision with `railway up --service web --environment staging`, or connect the intended repository and branch under web service Source. The repository root must contain `Dockerfile` and `Cargo.lock`. The `DOCKERFILE` builder compiles the pinned toolchain. `solo-tracker migrate` runs as a pre-deploy step under SQLx's migration lock. A failure stops traffic promotion. The web process runs `solo-tracker serve` as UID 65532 with `APP_ENV=production` and listens on the injected `PORT`. Railway polls `/readyz` for up to 120 seconds. [Pre-deploy commands](https://docs.railway.com/deployments/pre-deploy-command) · [Deployment health checks](https://docs.railway.com/deployments/healthchecks)
7. Check `/readyz`, open `/login`, deliver and redeem a real staging email, then create a project and an issue. Redeploy and confirm persistence. Check that the web role cannot write `_sqlx_migrations`, change `users.identity_current`, alter the schema or amend `account_deletions`. Check a second account cannot open the first account's issue URL. Record the source revision, built image identifier, last successful migration, checks and operator. Promote the reviewed revision to a separate production environment only after these checks and the recovery drill. To promote exactly the tested image, publish it to your private registry and import `image` from `railway/iac`. Set web's `source` to `image("registry/repository@sha256:…")`, removing its `build` stanza. Review the staging plan, run the same smoke tests against that digest, and use that digest in production. [Railway sources](https://docs.railway.com/infrastructure-as-code/reference)

### Optional services

Set each integration's environment-specific secret and add a corresponding `ctx.shared.NAME` reference to `web.env` in the IaC file when enabling it. Keep separate test and production accounts and recheck the staged variable diff. Configure `SUPPORT_EMAIL` and `STATUS_URL` with your own published destinations.

| Feature | Service configuration | Staging check |
| --- | --- | --- |
| Email | `RESEND_API_KEY`, `MAIL_FROM`, exact HTTPS `APP_BASE_URL` | Deliver a magic link from a verified sender and redeem it once. Keep `DEV_LOGIN_LINKS` disabled on hosted deployments. |
| GitHub App | `WORKSPACE_GITHUB_CLIENT_ID`, `WORKSPACE_GITHUB_CLIENT_SECRET`, `WORKSPACE_GITHUB_APP_SLUG`, `WORKSPACE_GITHUB_TOKEN_KEY` (32 random bytes, base64), `WORKSPACE_GITHUB_WEBHOOK_SECRET` | Use `<origin>/integrations/github/callback`, setup URL `<origin>/integrations`, webhook `<origin>/integrations/github/webhook`. Use read-only repository metadata, pull requests and Dependabot alerts. Subscribe to installation, installation-repository, repository, Dependabot alert and app-authorization events supported by the app. Verify signed event delivery, reconnect and revocation against a controlled repository. Keep the token encryption key stable across deploys. Rotation requires owner reauthorization or controlled offline re-encryption. Rotate webhook secrets on both sides and replay missed deliveries. |
| Stripe | `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`, `STRIPE_PRICE_PLUS`, `STRIPE_PRICE_MAX` | Register `<origin>/billing/webhook`, subscribe to Checkout session completion, expiry and asynchronous payment results, plus customer subscription, subscription schedule and invoice events. Configure the portal for the same recurring prices, immediate payment on upgrades, end-of-period downgrades/cancellation, email/contact updates, payment-method updates and receipts. Test checkout, cancellation and webhook retry in test mode. Enable `STRIPE_ENABLE_LIVE=true` only for an approved live rollout. |

## Observe and roll back

Set an external uptime check on `GET /readyz` every minute and notify the operator after five minutes of failure. Railway's probe governs deployment promotion. Watch web restarts, 5xx and response latency, PostgreSQL connections and storage, the integration page's last successful check, and off-account backup age. Alert on a stale archive, failed backup, unexpected restart loop or two missed hourly syncs. Verify an alert reaches the operator by deliberately failing a staging check. Do not export email links, authorization headers, cookies, request bodies, issue titles or note text to monitoring. [Railway health checks](https://docs.railway.com/deployments/healthchecks)

If a release fails its migration, inspect the failed deployment and migration version before retrying. For a bad application release, redeploy the last verified image compatible with the current schema. Apply an additive forward repair for schema defects. An image rollback leaves the database at its current schema. Preserve an incident snapshot and stop writes before any data cutover.

## Recover persistent data

| Failure | Internal data-loss target (RPO) | Internal recovery-time target (RTO) | Recovery path |
| --- | --- | --- | --- |
| Primary PostgreSQL failure | At most 15 minutes with healthy WAL archiving | Four hours, including cutover | Restore a sibling from Railway PITR. |
| Entire project or account lost | At most 24 hours with verified nightly off-account copies | Measure retrieval, transfer and service reconstruction in a full drill | Restore the independent encrypted archive into a new database. |

Enable **Point-in-time recovery** in the Postgres service's Backups tab. Wait until the first base archive exists and the date picker exposes a restorable range. Check `railway postgres pitr status --service postgres` and rehearse an isolated restore monthly. Selecting a timestamp creates a sibling with a separate volume. Re-enable PITR on a recovered sibling before cutover. Keep the original until acceptance. [Railway PITR](https://docs.railway.com/volumes/point-in-time-recovery)

### Independent copies

Use a private runner with access to the database and a destination controlled by a separate account. Install Bash, GNU coreutils, PostgreSQL 18 client tools and `age`. Store the age private identity and necessary infrastructure credentials outside both the app and primary Railway account. Give the backup job the age **public recipient** and write access to a private, versioned off-account destination with retention controls. Restrict delete permissions separately. The supplied scripts produce snapshots on a filesystem. Supply an independently backed mount or upload the completed directory and check its checksum after retrieval. A local directory alone leaves the archive on the primary host.

Set `PGHOST`, `PGPORT`, `PGUSER`, `PGDATABASE`, `PGPASSFILE` and `AGE_RECIPIENT` through the job's secret environment. `PGDATABASE` is a database name. Let the backup role read all application tables and sequences. Schedule the full backup nightly and keep recoverable versions through the advertised restore window. Separately copy the operational ledger after each erasure, verified sign-in-address recovery or billing shutdown change, or continuously from a protected change feed. It records opaque account IDs and event identifiers needed to fence an old sign-in address. Preserve successive ledger versions separately from the nightly archive. Monitor the latest successful off-account upload and the completeness of erasures through cutover.

```sh
ops/backup.sh /mnt/off-account/full
ops/backup.sh --deletions /mnt/off-account/deletions
```

The historical `--deletions` option now writes a v2 recovery ledger containing erasure markers, opaque verified-email-change markers and pending provider cancellations. It contains no replacement mailbox addresses. A successful command prints its UTC-stamped snapshot directory. Each contains an age-encrypted payload, manifest and SHA-256 checksums. A full archive includes image bytes and hash records in the same PostgreSQL snapshot. Schedule and verify the off-account uploads and ledger freshness on the deployed system.

### Isolate and cut over

1. Fence application writes and disable outbound email, GitHub jobs and Stripe callbacks in the recovery environment. Record the available restore timestamp. For whole-project recovery, retrieve the full archive and the latest independently retained deletion ledger from the trusted destination. Validate origin and retention history. Ensure all confirmed deletions through the incident are accounted for before exposing a restored workspace.
2. Select the recovery cluster with `PGHOST`, `PGPORT`, `PGUSER` and a protected `PGPASSFILE`. Use a role with `CREATEDB`, a maintenance database, and the separately held age identity. Replace the two snapshot-directory arguments below with the retrieved directories. The destination database name must be fresh. The three-argument restore requires a current-format independent ledger. It replays subsequent erasures, holds workspaces whose verified sign-in address changed after the snapshot, reconciles the billing shutdown queue, and purges restored sign-in and GitHub credentials. Keep held users offline until a trusted independent source confirms their latest verified address. An older erasure-only ledger cannot establish who controls the current mailbox. Restore also disables digest email and cancels pending deliveries, so each user must opt in again. In-app digests and sent delivery history are preserved.

   ```sh
   export PGDATABASE=postgres AGE_IDENTITY_FILE=/run/secrets/recovery-age-key
   ops/restore.sh /restore/full-snapshot workspace_recovered /restore/latest-deletions
   ```

3. An archive from before migration 0021 can be restored raw into an isolated database by omitting the ledger argument. Keep it offline. Run the current `solo-tracker migrate` as the owner without runtime grants, then re-capture that database using `--pitr-at` with the original verified snapshot time. Apply the independent v2 ledger when restoring that new archive into a fresh target.
4. For a Railway PITR sibling, keep it isolated until the same erasure replay and credential purge have succeeded. One way to use the same tested recovery routine is to take an encrypted full archive from the sibling using `ops/backup.sh --pitr-at YYYY-MM-DDTHH:MM:SSZ /mnt/off-account/full` with its actual, verified PITR target in UTC, then run the three-argument restore into a new database on the recovery cluster with the independently retrieved ledger. Include the extra copy time in the drill. Point the recovered web service at the verified database.
5. Run `ops/init-runtime.sh` with the restored database administrator, then run `solo-tracker migrate` with `APP_GRANT_RUNTIME=1` and the administrator URL on the isolated recovered database. Logical recovery omits source ACLs. Verify the web credential is the non-owner role. Match migrations and row counts to the recovery timestamp. Cross-check note/image references and hashes, a release's scoped and completed counts, owner boundaries, and access to an erased user's project. Verify old sessions and GitHub grants fail. Test fresh sign-in through a controlled mailbox and reauthorize GitHub. Keep pending checkout reservations and the billing cancellation queue. Reconcile customer subscriptions and post-snapshot Checkout Sessions with Stripe. Expire outstanding sessions or hold new checkout for 26 hours before reopening it. Re-enable billing webhooks and delivery jobs only after reconciliation. Confirm readiness and persistence with the restored backend in staging.
6. Point `WORKSPACE_RUNTIME_DATABASE_URL` and the migration-owner reference at the verified sibling/database, keep the old writer fenced, and review the web redeploy. Re-enable external callbacks and jobs only after revalidation. Record retrieval and restore durations, final restorable timestamp, cutover downtime, image count/hash checks, deletion-ledger watermark and backup health. Repeat monthly and after recovery-affecting migrations. These hosted targets require a measured staging drill before making a customer commitment.

### Repeat the local recovery drill

Point libpq at a disposable PostgreSQL 18 cluster with `CREATEDB` privileges. `ops/drill.sh` creates two randomly named fixture databases, applies the shipped migrations, backs up and restores a release and its linked image, checks two post-snapshot erasures, stale inboxes, billing shutdowns, session and GitHub credential invalidation, and withdrawn email consent. The script prints its timings and removes the fixture databases it created.

```sh
PGDATABASE=postgres PGHOST=127.0.0.1 ops/drill.sh
```

On November 20, 2026, the PostgreSQL 18.6 local fixture produced a 168,993-byte encrypted archive in 243 ms and restored it through the data checks in 709 ms. A separate HTTP check against a recovered database refused login to a superseded inbox and allowed an unaffected account to request its link. The drill also exercised a deletion ledger captured between an earlier PITR target and the later logical copy. A separate PostgreSQL 18.7 Docker fixture passed on a clean volume. These small local measurements exclude remote retrieval, Railway provisioning, provider reconciliation and traffic cutover.
