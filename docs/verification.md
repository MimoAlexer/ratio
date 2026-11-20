# Local qualification

November 20, 2026. These results are from the integrated application using synthetic accounts, PostgreSQL and browser-rendered HTML. The standalone design preview is separate.

| Check | Observed result |
|---|---|
| Rust build and static checks | Locked build, rustfmt check and Clippy for all targets with warnings denied passed. |
| Automated application tests | 231 library tests, 3 concurrent storage/quota integration tests and 6 HTTP transfer integration tests passed. The opt-in read benchmark also passed separately. |
| Chromium user journeys | 12 desktop core checks, 14 roadmap/release checks, 7 billing/isolation/export checks, and 57 touch/mobile page observations passed. The 12 desktop core checks also passed with JavaScript disabled. |
| Browser behavior exercised | Real local sign-in, project creation, checkpoint, independent manual critical flag, undo, note/issue creation, rename and lost anchor, revision conflict, milestone prerequisites, selective release, reopened issue between preview and publish, and archived project ZIP export. |
| Provider boundaries | GitHub and Stripe behavior used local HTTP fixtures and disposable PostgreSQL databases. Billing downgrade QA seeded the prior paid entitlement and exercised the real reconciler. Email request and response behavior was tested with a local Resend fixture. |
| Recovery | Encrypted PostgreSQL recovery was rehearsed on tiny disposable fixtures, including file integrity, removal of sessions/provider credentials, independent erasure and recovery markers, and email-consent handling. Actual Railway cutover and production-scale timing require a staged drill. |

## Authenticated read benchmark

The opt-in test rendered real HTML through loopback HTTP. It populated one workspace with five projects, 10,000 issues, 2,000 notes and 50,000 links, then used 25 independent signed-in sessions, 16 database connections and four Tokio workers. Each route received 50 warmups and 200 measured requests. The native development build returned 800 measured responses with zero errors.

| Route | Median | p95 |
|---|---:|---:|
| Home | 27.78 ms | 36.49 ms |
| Issue list | 46.20 ms | 64.91 ms |
| Issue detail | 41.26 ms | 49.73 ms |
| Search | 33.91 ms | 43.27 ms |

Repeat the test with a disposable PostgreSQL role that can create databases:

```bash
cargo test --locked --test read_load -- --ignored --nocapture
```

Hosted latency, field browser metrics, customer demand and forecast calibration require real use. Deployment instructions include production email, private repository and Stripe test-mode checks that must be performed with the operator’s provider accounts.

## Final container smoke

The pinned Linux image was built in Docker/Compose from application inputs with SHA-256 `edf4e849bed89342b3fb8306cbc8c6b15ed9a68538e1e78900b0812d9f680fd6`. The final image index was `sha256:6a4048b740eeb04f734f8f335cb0d1d8d6602ac27d89bcfe4db8b987e763d2a7`. It ran as UID/GID 65532 against PostgreSQL 18.7 using a separate migration owner and restricted runtime role. The checks covered a nondefault production port, secure cookies, readiness, 11 authenticated routes, content metering, and preservation of a signed-in session and its project after a full Compose recreation. Railway infrastructure configuration was evaluated locally. A Railway account and live provider credentials were not used.
