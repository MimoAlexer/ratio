use super::safe_next;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Method, Request, StatusCode, header},
};
use sqlx::PgPool;
use std::{collections::BTreeMap, net::SocketAddr};
use tower::ServiceExt;
use uuid::Uuid;

#[test]
fn only_pairing_codes_survive_the_signin_round_trip() {
    let pending = "/desktop/authorize?code=X24P-F5VH";
    assert_eq!(safe_next(pending), pending);
    for path in [
        "/desktop/authorize?code=X24P-F5VH&next=//foreign.example",
        "/desktop/authorize?code=../settings",
        "/desktop/authorize?code=ABC",
        "/desktop/finish",
    ] {
        assert_eq!(safe_next(path), "/");
    }
}

#[derive(Clone, Default)]
struct Client {
    peer_suffix: u8,
    cookies: BTreeMap<String, String>,
}

struct Page {
    status: StatusCode,
    location: String,
    html: String,
}

impl Client {
    async fn get(&mut self, app: &Router, path: &str) -> Page {
        self.send(app, Method::GET, path, &[]).await
    }

    async fn post(&mut self, app: &Router, path: &str, fields: &[(&str, &str)]) -> Page {
        self.send(app, Method::POST, path, fields).await
    }

    async fn send(
        &mut self,
        app: &Router,
        method: Method,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Page {
        let cookies = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        let data = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish();
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1")
            .header(header::ORIGIN, "http://127.0.0.1")
            .header(header::COOKIE, cookies)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .extension(ConnectInfo(SocketAddr::from((
                [127, 0, 1, self.peer_suffix],
                23000,
            ))))
            .body(Body::from(data))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        for cookie in resp.headers().get_all(header::SET_COOKIE).iter() {
            let (name, value) = cookie
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .split_once('=')
                .unwrap();
            if value.is_empty() {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.into(), value.into());
            }
        }
        let status = resp.status();
        let location = resp
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let html = String::from_utf8(
            to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        Page {
            status,
            location,
            html,
        }
    }
}

fn value(html: &str, key: &str) -> String {
    let marker = format!("name=\"{key}\" value=\"");
    html.split_once(&marker)
        .unwrap_or_else(|| panic!("missing form field {key}"))
        .1
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

async fn start(app: &Router, desktop: &mut Client) -> String {
    let form = desktop.get(app, "/desktop/start").await;
    assert_eq!(form.status, StatusCode::OK);
    let csrf = value(&form.html, "csrf");
    let bad = desktop
        .post(app, "/desktop/start", &[("csrf", "wrong")])
        .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    let issued = desktop
        .post(app, "/desktop/start", &[("csrf", &csrf)])
        .await;
    assert_eq!(issued.status, StatusCode::SEE_OTHER);
    assert_eq!(issued.location, "/desktop/check");
    let check = desktop.get(app, &issued.location).await;
    assert_eq!(check.status, StatusCode::OK);
    let marker = "href=\"/desktop/authorize?code=";
    let code = check
        .html
        .split_once(marker)
        .expect("browser link")
        .1
        .split('"')
        .next()
        .unwrap();
    assert_eq!(code.len(), 9);
    assert_eq!(code.as_bytes()[4], b'-');
    let browser_link = check
        .html
        .split('<')
        .find(|tag| tag.starts_with("a ") && tag.contains(marker))
        .unwrap();
    assert!(
        browser_link.contains("target=\"_blank\""),
        "native browser handoff requires a new window"
    );
    assert!(
        code.bytes()
            .enumerate()
            .all(|(index, ch)| index == 4 || ch.is_ascii_uppercase() || ch.is_ascii_digit())
    );
    assert!(!check.html.contains(&desktop.cookies["workspace_device"]));
    code.into()
}

// Model an email provider delivering a single challenge to the browser. The login and
// confirmation pages, cookies, CSRF checks, return destination and session writes are real.
async fn sign_in(
    app: &Router,
    db: &PgPool,
    browser: &mut Client,
    login_url: &str,
    email: &str,
) -> String {
    let form = browser.get(app, login_url).await;
    let destination = value(&form.html, "next");
    let token = super::token();
    sqlx::query("INSERT INTO login_challenges(token_hash,email,return_to) VALUES($1,$2,$3)")
        .bind(super::digest(&token))
        .bind(email)
        .bind(&destination)
        .execute(db)
        .await
        .unwrap();
    let confirm = browser
        .get(app, &format!("/login/confirm?token={token}"))
        .await;
    assert_eq!(confirm.status, StatusCode::OK);
    let csrf = value(&confirm.html, "csrf");
    let signed = browser
        .post(app, "/login/confirm", &[("token", &token), ("csrf", &csrf)])
        .await;
    assert_eq!(signed.status, StatusCode::SEE_OTHER);
    signed.location
}

async fn approve(app: &Router, browser: &mut Client, code: &str) {
    let page = browser
        .get(app, &format!("/desktop/authorize?code={code}"))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = value(&page.html, "csrf");
    let approved = browser
        .post(
            app,
            "/desktop/authorize",
            &[("csrf", &csrf), ("code", code)],
        )
        .await;
    assert_eq!(approved.status, StatusCode::OK);
}

fn assert_denied(page: &Page) {
    assert!(
        page.status.is_client_error(),
        "unexpected status {}",
        page.status
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn pairing_requires_browser_approval_and_confirmation_on_the_originating_desktop(db: PgPool) {
    let app = crate::app(crate::AppState::for_test(db.clone()));
    let email = "reader@example.invalid";
    let mut desktop = Client::default();
    let mut browser = Client::default();
    let code = start(&app, &mut desktop).await;
    let route = format!("/desktop/authorize?code={code}");
    let login = browser.get(&app, &route).await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    let target = sign_in(&app, &db, &mut browser, &login.location, email).await;
    assert_eq!(target, route);
    let browser_session = browser.cookies["workspace_session"].clone();
    sqlx::query("UPDATE user_sessions SET created_at=now()-interval '1 hour' WHERE token_hash=$1")
        .bind(super::digest(&browser_session))
        .execute(&db)
        .await
        .unwrap();

    let preview = browser.get(&app, &target).await;
    assert_eq!(preview.status, StatusCode::OK);
    assert!(preview.html.contains(&code));
    let browser_csrf = value(&preview.html, "csrf");
    let bad = browser
        .post(
            &app,
            "/desktop/authorize",
            &[("csrf", "wrong"), ("code", &code)],
        )
        .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    assert!(
        !desktop
            .get(&app, "/desktop/check")
            .await
            .html
            .contains(email)
    );
    assert!(
        desktop
            .get(&app, "/settings")
            .await
            .location
            .starts_with("/login")
    );
    approve(&app, &mut browser, &code).await;
    let mut other_browser = Client::default();
    sign_in(
        &app,
        &db,
        &mut other_browser,
        "/login",
        "other@example.invalid",
    )
    .await;
    let other_csrf = value(&other_browser.get(&app, "/settings").await.html, "csrf");
    let conflicting = other_browser
        .post(
            &app,
            "/desktop/authorize",
            &[("csrf", &other_csrf), ("code", &code)],
        )
        .await;
    assert_denied(&conflicting);
    let confirmation = desktop.get(&app, "/desktop/check").await;
    assert_eq!(confirmation.status, StatusCode::OK);
    assert!(confirmation.html.contains(email));
    assert!(
        desktop
            .get(&app, "/settings")
            .await
            .location
            .starts_with("/login")
    );
    let desktop_csrf = value(&confirmation.html, "csrf");
    let owner = value(&confirmation.html, "account");
    let valid = [("csrf", desktop_csrf.as_str()), ("account", owner.as_str())];
    let bad_csrf = desktop
        .post(
            &app,
            "/desktop/finish",
            &[("csrf", "wrong"), ("account", &owner)],
        )
        .await;
    assert_eq!(bad_csrf.status, StatusCode::FORBIDDEN);
    let wrong_owner = desktop
        .post(
            &app,
            "/desktop/finish",
            &[
                ("csrf", &desktop_csrf),
                ("account", &Uuid::new_v4().to_string()),
            ],
        )
        .await;
    assert_denied(&wrong_owner);
    let mut wrong_device = desktop.clone();
    wrong_device
        .cookies
        .insert("workspace_device".into(), super::token());
    assert_denied(&wrong_device.post(&app, "/desktop/finish", &valid).await);
    assert!(!wrong_device.cookies.contains_key("workspace_session"));

    let mut duplicate = desktop.clone();
    let (first, second) = tokio::join!(
        desktop.post(&app, "/desktop/finish", &valid),
        duplicate.post(&app, "/desktop/finish", &valid)
    );
    let (mut winner, loser) = if first.status == StatusCode::SEE_OTHER {
        assert_eq!(first.location, "/issues");
        (desktop, second)
    } else {
        assert_eq!(second.status, StatusCode::SEE_OTHER);
        assert_eq!(second.location, "/issues");
        (duplicate, first)
    };
    assert_denied(&loser);
    assert_eq!(winner.get(&app, "/desktop/start").await.location, "/issues");
    let desktop_session = winner.cookies["workspace_session"].clone();
    assert_ne!(desktop_session, browser_session);
    assert!(winner.get(&app, "/settings").await.html.contains(email));
    let freshness_kept: bool = sqlx::query_scalar(
        "SELECT d.created_at=b.created_at AND d.expires_at=b.expires_at FROM user_sessions d \
         JOIN user_sessions b ON b.token_hash=$2 WHERE d.token_hash=$1",
    )
    .bind(super::digest(&desktop_session))
    .bind(super::digest(&browser_session))
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(
        freshness_kept,
        "pairing must not lift step-up authentication restrictions"
    );
    let revoked = browser
        .post(
            &app,
            "/settings/sessions/revoke",
            &[("csrf", &browser_csrf)],
        )
        .await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);
    assert!(
        winner
            .get(&app, "/settings")
            .await
            .location
            .starts_with("/login")
    );
    assert!(browser.get(&app, "/settings").await.html.contains(email));
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_and_revoked_approvals_cannot_open_a_session(db: PgPool) {
    let app = crate::app(crate::AppState::for_test(db.clone()));
    let mut blocked_restarts = Vec::new();
    for reason in [
        "expired",
        "browser-signed-out",
        "browser-expired",
        "others-revoked",
        "identity-held",
    ] {
        let mut desktop = Client::default();
        let code = start(&app, &mut desktop).await;
        let mut browser = Client::default();
        let email = format!("{reason}@example.invalid");
        sign_in(&app, &db, &mut browser, "/login", &email).await;
        approve(&app, &mut browser, &code).await;
        let confirm = desktop.get(&app, "/desktop/check").await;
        assert_eq!(confirm.status, StatusCode::OK);
        let csrf = value(&confirm.html, "csrf");
        let owner = value(&confirm.html, "account");
        match reason {
            "expired" => {
                sqlx::query("UPDATE desktop_devices SET expires_at=now()-interval '1 second' WHERE device_hash=$1")
                  .bind(super::digest(&desktop.cookies["workspace_device"])).execute(&db).await.unwrap();
            }
            "browser-expired" => {
                sqlx::query(
                    "UPDATE user_sessions SET created_at=now()-interval '1 hour', \
                  expires_at=now()-interval '1 minute' WHERE token_hash=$1",
                )
                .bind(super::digest(&browser.cookies["workspace_session"]))
                .execute(&db)
                .await
                .unwrap();
            }
            "identity-held" => {
                sqlx::query("UPDATE users SET identity_current=false WHERE email=$1")
                    .bind(&email)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            action => {
                let route = if action == "browser-signed-out" {
                    "/logout"
                } else {
                    "/settings/sessions/revoke"
                };
                let settings = browser.get(&app, "/settings").await;
                let browser_csrf = value(&settings.html, "csrf");
                assert_eq!(
                    browser
                        .post(&app, route, &[("csrf", &browser_csrf)])
                        .await
                        .status,
                    StatusCode::SEE_OTHER
                );
            }
        }
        let finish = desktop
            .post(
                &app,
                "/desktop/finish",
                &[("csrf", &csrf), ("account", &owner)],
            )
            .await;
        assert!(
            finish.status.is_client_error(),
            "{reason} approved a desktop: {}",
            finish.status
        );
        assert!(!desktop.cookies.contains_key("workspace_session"));
        let unavailable = desktop.get(&app, "/desktop/check").await;
        assert_eq!(unavailable.status, StatusCode::GONE, "{reason}");
        let restart = desktop.get(&app, "/desktop/start").await;
        if restart.status != StatusCode::OK {
            blocked_restarts.push((reason, restart.status, restart.location));
            continue;
        }
        let previous_secret = desktop.cookies["workspace_device"].clone();
        let nonce = value(&restart.html, "csrf");
        let replacement = desktop
            .post(&app, "/desktop/start", &[("csrf", &nonce)])
            .await;
        assert_eq!(replacement.location, "/desktop/check");
        let check = desktop.get(&app, &replacement.location).await;
        assert_eq!(check.status, StatusCode::OK);
        assert!(check.html.contains("href=\"/desktop/authorize?code="));
        assert_ne!(desktop.cookies["workspace_device"], previous_secret);
        assert!(
            desktop
                .get(&app, "/settings")
                .await
                .location
                .starts_with("/login")
        );
    }
    assert!(
        blocked_restarts.is_empty(),
        "unusable pairing requests blocked a fresh start: {blocked_restarts:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn repeated_creation_and_code_guesses_are_throttled(db: PgPool) {
    let app = crate::app(crate::AppState::for_test(db.clone()));
    let mut desktop = Client::default();
    let start_form = desktop.get(&app, "/desktop/start").await;
    let nonce = value(&start_form.html, "csrf");
    for _ in 0..40 {
        let issued = desktop
            .post(&app, "/desktop/start", &[("csrf", &nonce)])
            .await;
        assert_eq!(issued.status, StatusCode::SEE_OTHER);
    }
    let live_secret = desktop.cookies["workspace_device"].clone();
    let denied = desktop
        .post(&app, "/desktop/start", &[("csrf", &nonce)])
        .await;
    assert_eq!(denied.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(desktop.cookies["workspace_device"], live_secret);
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM desktop_devices")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(pending, 1);

    let mut other_desktop = Client {
        peer_suffix: 1,
        ..Client::default()
    };
    let real_code = start(&app, &mut other_desktop).await;
    let mut browser = Client::default();
    sign_in(&app, &db, &mut browser, "/login", "guesses@example.invalid").await;
    let csrf = value(&browser.get(&app, "/settings").await.html, "csrf");
    for _ in 0..30 {
        let guessed = browser
            .post(
                &app,
                "/desktop/authorize",
                &[("csrf", &csrf), ("code", "unknown")],
            )
            .await;
        assert_eq!(guessed.status, StatusCode::GONE);
    }
    browser.peer_suffix = 2;
    let denied = browser
        .post(
            &app,
            "/desktop/authorize",
            &[("csrf", &csrf), ("code", &real_code)],
        )
        .await;
    assert_eq!(denied.status, StatusCode::TOO_MANY_REQUESTS);
    let mut other_browser = Client {
        peer_suffix: 1,
        ..Client::default()
    };
    sign_in(
        &app,
        &db,
        &mut other_browser,
        "/login",
        "approved@example.invalid",
    )
    .await;
    approve(&app, &mut other_browser, &real_code).await;
    let confirm = other_desktop.get(&app, "/desktop/check").await;
    assert!(confirm.html.contains("approved@example.invalid"));
}

#[test]
fn github_browser_handoff_retains_a_single_canonical_owner() {
    let route = "/integrations/github/browser?owner=24aafd3d-d905-43cc-8cc2-278074e3ff79";
    assert_eq!(safe_next(route), route);
    for path in [
        "/integrations/github/browser",
        "/integrations/github/browser?owner=",
        "/integrations/github/browser?owner=24AAFD3D-D905-43CC-8CC2-278074E3FF79",
        "/integrations/github/browser?owner=24aafd3dd90543cc8cc2278074e3ff79",
        "/integrations/github/browser?owner=24aafd3d-d905-43cc-8cc2-278074e3ff79&next=//foreign.example",
        "/integrations/github/browser?next=24aafd3d-d905-43cc-8cc2-278074e3ff79",
    ] {
        assert_eq!(safe_next(path), "/", "{path}");
    }
}
