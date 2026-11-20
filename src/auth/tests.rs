use super::{digest, normalized_email, redeem, safe_next, session, token};
use sqlx::PgPool;
use std::collections::HashSet;
use uuid::Uuid;

#[test]
fn tokens_are_unique_url_safe_and_stored_as_digests() {
    let mut observed = HashSet::new();
    for _ in 0..128 {
        let raw = token();
        assert!(raw.len() >= 32);
        assert!(
            raw.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(digest(&raw).len(), 32);
        assert!(observed.insert(digest(&raw)));
    }
}

#[test]
fn return_targets_stay_on_this_origin() {
    let issue = format!("/issues/{}", Uuid::new_v4());
    assert_eq!(safe_next(&issue), issue);
    let editor = format!("/notes/{}/edit", Uuid::new_v4());
    assert_eq!(safe_next(&editor), editor);
    let drawing = format!("/canvas/{}", Uuid::new_v4());
    assert_eq!(safe_next(&drawing), drawing);
    let boards = format!("/canvas?project={}", Uuid::new_v4());
    assert_eq!(safe_next(&boards), boards);
    let new_board = format!("{boards}&new=true");
    assert_eq!(safe_next(&new_board), new_board);
    assert_eq!(
        safe_next("/canvas?new=true&project=invalid"),
        "/canvas?new=true"
    );
    for target in [
        "https://elsewhere.example",
        "//elsewhere.example/path",
        "/\\elsewhere.example",
        "/x\r\ny",
        "",
    ] {
        assert_eq!(
            safe_next(target),
            "/",
            "unexpected return target {target:?}"
        );
    }
}

#[test]
fn login_addresses_are_trimmed_and_invalid_input_is_rejected() {
    assert_eq!(
        normalized_email("  reader+archive@EXAMPLE.COM ").as_deref(),
        Some("reader+archive@example.com")
    );
    for address in [
        "",
        "@example.com",
        "reader@",
        "reader@@example.com",
        "a\r\nb@example.com",
    ] {
        assert!(
            normalized_email(address).is_none(),
            "accepted address {address:?}"
        );
    }
}

async fn challenge(db: &PgPool, email: &str, next: &str, valid_minutes: i32) -> String {
    let raw = token();
    sqlx::query(
        "INSERT INTO login_challenges (token_hash,email,return_to,expires_at)
         VALUES ($1,$2,$3,now() + make_interval(mins => $4))",
    )
    .bind(digest(&raw))
    .bind(email)
    .bind(next)
    .bind(valid_minutes)
    .execute(db)
    .await
    .expect("insert synthetic challenge");
    raw
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_redemption_session_rotation_expiry_and_isolation(db: PgPool) {
    let first_email = format!("auth-{}@example.invalid", Uuid::new_v4());
    let second_email = format!("auth-{}@example.invalid", Uuid::new_v4());
    let destination = format!("/issues/{}", Uuid::new_v4());
    let first = challenge(&db, &first_email, &destination, 15).await;
    let (a, b) = tokio::join!(redeem(&db, &first, None), redeem(&db, &first, None));
    let successes: Vec<_> = [a.expect("first attempt"), b.expect("second attempt")]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        successes.len(),
        1,
        "exactly one concurrent redemption succeeds"
    );
    let (first_token, returned_to) = &successes[0];
    assert_eq!(returned_to, &destination);
    let first_session = session(&db, first_token)
        .await
        .unwrap()
        .expect("first session");
    assert_eq!(first_session.email, first_email);
    assert!(first_session.check_csrf(&first_session.csrf).is_ok());
    assert!(first_session.check_csrf("").is_err());

    let second = challenge(&db, &second_email, "/", 15).await;
    let (second_token, _) = redeem(&db, &second, None).await.unwrap().unwrap();
    let second_session = session(&db, &second_token).await.unwrap().unwrap();
    assert_eq!(second_session.email, second_email);
    assert_ne!(first_session.user_id, second_session.user_id);
    assert!(first_session.check_csrf(&second_session.csrf).is_err());
    assert!(session(&db, &token()).await.unwrap().is_none());

    let rotation = challenge(&db, &first_email, "/", 15).await;
    let (new_token, _) = redeem(&db, &rotation, Some(first_token))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(&new_token, first_token);
    let rotated = session(&db, &new_token).await.unwrap().unwrap();
    assert_eq!(rotated.user_id, first_session.user_id);
    assert!(rotated.check_csrf(&first_session.csrf).is_err());
    assert!(session(&db, first_token).await.unwrap().is_none());
    assert!(session(&db, &second_token).await.unwrap().is_some());

    let expired = challenge(&db, &second_email, "/", -1).await;
    assert!(redeem(&db, &expired, None).await.unwrap().is_none());
    sqlx::query(
        "UPDATE user_sessions SET created_at=now()-interval '2 days',
         expires_at=now()-interval '1 day' WHERE token_hash=$1",
    )
    .bind(digest(&new_token))
    .execute(&db)
    .await
    .unwrap();
    assert!(session(&db, &new_token).await.unwrap().is_none());
    assert!(session(&db, &second_token).await.unwrap().is_some());

    sqlx::query("DELETE FROM login_challenges WHERE email IN ($1,$2)")
        .bind(&first_email)
        .bind(&second_email)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE email IN ($1,$2)")
        .bind(first_email)
        .bind(second_email)
        .execute(&db)
        .await
        .unwrap();
    assert!(session(&db, &second_token).await.unwrap().is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn magic_link_prefetch_needs_explicit_post_and_logout_revokes_cookie(db: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
        routing::get,
    };
    use tower::ServiceExt;
    let email = "owner@example.invalid";
    let issue = format!("/issues/{}", Uuid::new_v4());
    let login = challenge(&db, email, &issue, 15).await;
    let state = crate::AppState::for_test(db.clone());
    let production = state.config.production;
    let app = super::routes()
        .route(
            "/issues/{id}",
            get(|auth: super::Auth| async move { auth.email }),
        )
        .with_state(state);
    let check = || {
        Request::builder()
            .uri(format!("/login/confirm?token={login}"))
            .body(Body::empty())
            .unwrap()
    };
    let first = app.clone().oneshot(check()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let second = app.clone().oneshot(check()).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM user_sessions")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    let cookie = second.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let nonce = cookie.split_once('=').unwrap().1;
    let post = |path: &str, pair: &str, form: String| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::COOKIE, pair)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap()
    };
    let rejected = app
        .clone()
        .oneshot(post(
            "/login/confirm",
            cookie,
            format!("token={login}&csrf=wrong"),
        ))
        .await
        .unwrap();
    assert!(rejected.status().is_client_error());
    let verify = || {
        post(
            "/login/confirm",
            cookie,
            format!("token={login}&csrf={nonce}"),
        )
    };
    let signed_in = app.clone().oneshot(verify()).await.unwrap();
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        signed_in.headers()[header::LOCATION].to_str().unwrap(),
        issue
    );
    assert_eq!(
        app.clone().oneshot(verify()).await.unwrap().status(),
        StatusCode::GONE
    );
    let name = super::session_name(production);
    let session_cookie = signed_in
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find(|p| p.starts_with(&format!("{name}=")))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let raw = session_cookie.split_once('=').unwrap().1;
    let page = || {
        Request::builder()
            .uri(&issue)
            .header(header::COOKIE, session_cookie)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(page()).await.unwrap().status(),
        StatusCode::OK
    );
    let csrf = session(&db, raw).await.unwrap().unwrap().csrf;
    let logout = app
        .clone()
        .oneshot(post("/logout", session_cookie, format!("csrf={csrf}")))
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::SEE_OTHER);
    let cleanup_cookie = logout
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(super::cleanup_cookie_name(production)))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let login = |cookie: Option<&str>| {
        let mut request = Request::builder().uri("/login?result=signed-out");
        if let Some(value) = cookie {
            request = request.header(header::COOKIE, value)
        }
        request.body(Body::empty()).unwrap()
    };
    let marker_page = app
        .clone()
        .oneshot(login(Some(cleanup_cookie)))
        .await
        .unwrap();
    assert!(
        marker_page
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|v| v.to_str().unwrap().contains("Max-Age=0"))
    );
    let body = axum::body::to_bytes(marker_page.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("data-clear-note-drafts")
    );
    let spoofed = app.clone().oneshot(login(None)).await.unwrap();
    let body = axum::body::to_bytes(spoofed.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        !std::str::from_utf8(&body)
            .unwrap()
            .contains("data-clear-note-drafts")
    );
    let expired = app.oneshot(page()).await.unwrap();
    assert_eq!(expired.status(), StatusCode::SEE_OTHER);
    assert!(
        expired.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .ends_with(&issue.replace("/", "%2F"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn request_budget_resets_after_the_window(db: PgPool) {
    let mut tx = db.begin().await.unwrap();
    assert!(super::budget(&mut tx, "limit-fixture", 1).await.unwrap());
    assert!(!super::budget(&mut tx, "limit-fixture", 1).await.unwrap());
    sqlx::query(
        "UPDATE login_rate_limits SET started_at=now()-interval '16 minutes' WHERE bucket=$1",
    )
    .bind(digest("limit-fixture"))
    .execute(&mut *tx)
    .await
    .unwrap();
    assert!(super::budget(&mut tx, "limit-fixture", 1).await.unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn timezone_and_confirmed_deletion_are_owner_scoped(db: PgPool) {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;
    let owner_email = "settings@example.invalid";
    let other_email = "other@example.invalid";
    let first_link = challenge(&db, owner_email, "/", 15).await;
    let owner_raw = redeem(&db, &first_link, None).await.unwrap().unwrap().0;
    let owner = session(&db, &owner_raw).await.unwrap().unwrap();
    let other_link = challenge(&db, other_email, "/", 15).await;
    let other_raw = redeem(&db, &other_link, None).await.unwrap().unwrap().0;
    let outstanding = challenge(&db, owner_email, "/", 15).await;
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Example','TMP') RETURNING id",
    )
    .bind(owner.user_id)
    .fetch_one(&db)
    .await
    .unwrap();
    let state = crate::AppState::for_test(db.clone());
    let cookie = format!(
        "{}={owner_raw}",
        super::session_name(state.config.production)
    );
    let app = super::routes().with_state(state);
    let post = |route: &str, body: &str| {
        Request::builder()
            .method("POST")
            .uri(route)
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("csrf={}&{body}", owner.csrf)))
            .unwrap()
    };
    let invalid = app
        .clone()
        .oneshot(post("/settings/timezone", "timezone=Unknown%2FZone"))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let good = app
        .clone()
        .oneshot(post("/settings/timezone", "timezone=Europe%2FZurich"))
        .await
        .unwrap();
    assert_eq!(good.status(), StatusCode::SEE_OTHER);
    let zone: String = sqlx::query_scalar("SELECT timezone FROM users WHERE id=$1")
        .bind(owner.user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(zone, "Europe/Zurich");
    let wrong = app
        .clone()
        .oneshot(post("/settings/delete", "email=other%40example.invalid"))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert!(session(&db, &owner_raw).await.unwrap().is_some());
    let removed = app
        .oneshot(post("/settings/delete", "email=settings%40example.invalid"))
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::SEE_OTHER);
    assert!(session(&db, &owner_raw).await.unwrap().is_none());
    assert!(redeem(&db, &outstanding, None).await.unwrap().is_none());
    assert!(session(&db, &other_raw).await.unwrap().is_some());
    let marker: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account_deletions WHERE user_id=$1)")
            .bind(owner.user_id)
            .fetch_one(&db)
            .await
            .unwrap();
    let project_remains: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1)")
            .bind(project)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(marker);
    assert!(!project_remains);
}

#[sqlx::test(migrations = "./migrations")]
async fn restore_hold_blocks_superseded_mailbox_without_blocking_other_workspaces(db: PgPool) {
    let stale = "former@example.test";
    let other = "current@example.test";
    let initial = challenge(&db, stale, "/", 15).await;
    let raw = redeem(&db, &initial, None).await.unwrap().unwrap().0;
    let owner = session(&db, &raw).await.unwrap().unwrap().user_id;
    let login = challenge(&db, stale, "/", 15).await;
    sqlx::query("UPDATE users SET identity_current=false WHERE id=$1")
        .bind(owner)
        .execute(&db)
        .await
        .unwrap();
    assert!(session(&db, &raw).await.unwrap().is_none());
    assert!(redeem(&db, &login, None).await.unwrap().is_none());
    let other_link = challenge(&db, other, "/", 15).await;
    let other_raw = redeem(&db, &other_link, None).await.unwrap().unwrap().0;
    assert!(session(&db, &other_raw).await.unwrap().is_some());
}
