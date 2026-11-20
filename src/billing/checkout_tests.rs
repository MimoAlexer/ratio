use super::{Plan, Stripe, start_checkout};
use crate::{AppError, AppState, auth::Auth};
use axum::{
    Form, Json, Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::LOCATION},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::Utc;
use http_body_util::{Full, Limited};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Default)]
struct Processor {
    owner: Uuid,
    customer_posts: usize,
    sessions: HashMap<String, Value>,
    checkout_keys: Vec<String>,
    drop_next_response: bool,
    active_subscription: bool,
}

type Fixture = Arc<Mutex<Processor>>;

fn monthly_price(id: &str) -> Value {
    let amount = match id {
        "price_plus" => 500,
        "price_max" => 1500,
        _ => panic!("unknown fixture price {id}"),
    };
    json!({"id":id, "unit_amount":amount, "currency":"usd", "type":"recurring",
           "billing_scheme":"per_unit", "livemode":false, "active":true,
           "recurring":{"interval":"month", "interval_count":1, "usage_type":"licensed"}})
}

async fn create_customer(
    State(fixture): State<Fixture>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    let mut processor = fixture.lock().unwrap();
    processor.customer_posts += 1;
    assert_eq!(
        headers["idempotency-key"],
        format!("workspace-customer-{}", processor.owner)
    );
    assert_eq!(form["metadata[workspace_id]"], processor.owner.to_string());
    Json(json!({"id":"cus_fixture","livemode":false,"metadata":{"workspace_id":processor.owner}}))
}

async fn subscriptions(State(fixture): State<Fixture>) -> Json<Value> {
    let processor = fixture.lock().unwrap();
    let data = if processor.active_subscription {
        vec![
            json!({"id":"sub_fixture", "customer":"cus_fixture","livemode":false,
            "metadata":{"workspace_id":processor.owner}, "status":"active",
            "items":{"has_more":false,"data":[{"id":"si_fixture","quantity":1,"price":monthly_price("price_plus")}]},
            "current_period_end":Utc::now().timestamp()+3600, "latest_invoice":null}),
        ]
    } else {
        Vec::new()
    };
    Json(json!({"has_more":false,"data":data}))
}

async fn create_session(
    State(fixture): State<Fixture>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let mut processor = fixture.lock().unwrap();
    let key = headers["idempotency-key"].to_str().unwrap().to_owned();
    let sequence = processor.sessions.len() + 1;
    assert_eq!(form["customer"], "cus_fixture");
    processor.checkout_keys.push(key.clone());
    let session = processor
        .sessions
        .entry(key)
        .or_insert_with(|| {
            json!({
                "id":format!("cs_test_{sequence}"), "livemode":false,
                "url":format!("https://checkout.stripe.com/c/pay/{sequence}"),
                "customer":"cus_fixture","mode":"subscription","status":"open",
                "expires_at":Utc::now().timestamp()+3600,"price":form["line_items[0][price]"]
            })
        })
        .clone();
    assert_eq!(session["price"], form["line_items[0][price]"]);
    if std::mem::take(&mut processor.drop_next_response) {
        // The provider already created this session. Break the HTTP body before
        // the app can receive its ID, as can happen when the connection resets.
        let body = Full::new(Bytes::from(session.to_string()));
        return Response::new(Body::new(Limited::new(body, 1)));
    }
    Json(session).into_response()
}

async fn session_status(State(fixture): State<Fixture>, Path(id): Path<String>) -> Json<Value> {
    let processor = fixture.lock().unwrap();
    let session = processor
        .sessions
        .values()
        .find(|s| s["id"] == id)
        .expect("known session");
    Json(session.clone())
}

async fn setup(db: sqlx::PgPool) -> (AppState, Stripe, Auth, Fixture) {
    let email = "checkout-test@example.invalid";
    let owner = sqlx::query_scalar("INSERT INTO users(email) VALUES($1) RETURNING id")
        .bind(email)
        .fetch_one(&db)
        .await
        .unwrap();
    let processor = Arc::new(Mutex::new(Processor {
        owner,
        ..Default::default()
    }));
    let app = Router::new()
        .route("/customers", post(create_customer))
        .route("/subscriptions", get(subscriptions))
        .route(
            "/prices/{id}",
            get(|Path(id): Path<String>| async move { Json(monthly_price(&id)) }),
        )
        .route("/checkout/sessions", post(create_session))
        .route("/checkout/sessions/{id}", get(session_status))
        .with_state(processor.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stripe = Stripe::test_endpoint(format!("http://{}", listener.local_addr().unwrap()));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let auth = Auth {
        user_id: owner,
        email: email.into(),
        csrf: String::new(),
    };
    (AppState::for_test(db), stripe, auth, processor)
}

fn location(redirect: Redirect) -> String {
    let response = redirect.into_response();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response.headers()[LOCATION].to_str().unwrap().to_string()
}

async fn plan(state: &AppState, owner: Uuid) -> String {
    sqlx::query_scalar("SELECT plan FROM users WHERE id=$1")
        .bind(owner)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn lost_checkout_response_and_concurrent_retries_create_one_hosted_session(db: sqlx::PgPool) {
    let (state, stripe, auth, fixture) = setup(db).await;
    fixture.lock().unwrap().drop_next_response = true;
    assert!(matches!(
        start_checkout(&state, &stripe, &auth, Plan::Plus).await,
        Err(AppError::Unavailable(_))
    ));
    let reservation: (Uuid, String, Option<String>) = sqlx::query_as(
        "SELECT pending_key, pending_plan, pending_session FROM billing_accounts WHERE owner_id=$1",
    )
    .bind(auth.user_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(reservation.1, "plus");
    assert_eq!(reservation.2, None);
    assert!(matches!(
        start_checkout(&state, &stripe, &auth, Plan::Max).await,
        Err(AppError::Conflict(_))
    ));

    let (first, second) = tokio::join!(
        start_checkout(&state, &stripe, &auth, Plan::Plus),
        start_checkout(&state, &stripe, &auth, Plan::Plus),
    );
    let first = location(first.unwrap());
    assert_eq!(first, "https://checkout.stripe.com/c/pay/1");
    assert_eq!(first, location(second.unwrap()));
    assert_eq!(plan(&state, auth.user_id).await, "free");
    let processor = fixture.lock().unwrap();
    assert_eq!(processor.customer_posts, 1);
    assert_eq!(processor.sessions.len(), 1);
    assert_eq!(processor.checkout_keys, vec![reservation.0.to_string(); 2]);
}

#[sqlx::test]
async fn open_checkout_blocks_switch_and_expiry_allows_new_plan(db: sqlx::PgPool) {
    let (state, stripe, auth, fixture) = setup(db).await;
    let first = location(
        start_checkout(&state, &stripe, &auth, Plan::Plus)
            .await
            .unwrap(),
    );
    // Stripe still reports an open session even if the local clock reaches
    // the advertised expiry first. Its URL must remain the sole checkout.
    sqlx::query("UPDATE billing_accounts SET pending_expires_at=now()-interval '1 second' WHERE owner_id=$1")
        .bind(auth.user_id).execute(&state.db).await.unwrap();
    assert!(matches!(
        start_checkout(&state, &stripe, &auth, Plan::Max).await,
        Err(AppError::Conflict(_))
    ));
    fixture
        .lock()
        .unwrap()
        .sessions
        .values_mut()
        .next()
        .unwrap()["status"] = json!("expired");
    let next = location(
        start_checkout(&state, &stripe, &auth, Plan::Max)
            .await
            .unwrap(),
    );
    assert_ne!(first, next);
    let pending: String =
        sqlx::query_scalar("SELECT pending_plan FROM billing_accounts WHERE owner_id=$1")
            .bind(auth.user_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(pending, "max");
    assert_eq!(plan(&state, auth.user_id).await, "free");
    let processor = fixture.lock().unwrap();
    assert_eq!(processor.sessions.len(), 2);
    assert_ne!(processor.checkout_keys[0], processor.checkout_keys[1]);
}

#[sqlx::test]
async fn existing_external_subscription_blocks_new_checkout_without_unproven_access(
    db: sqlx::PgPool,
) {
    let (state, stripe, auth, fixture) = setup(db).await;
    assert_eq!(
        location(
            start_checkout(&state, &stripe, &auth, Plan::Plus)
                .await
                .unwrap()
        ),
        "https://checkout.stripe.com/c/pay/1"
    );
    fixture
        .lock()
        .unwrap()
        .sessions
        .values_mut()
        .next()
        .unwrap()["status"] = json!("complete");
    assert!(matches!(
        start_checkout(&state, &stripe, &auth, Plan::Plus).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(plan(&state, auth.user_id).await, "free");
    assert_eq!(fixture.lock().unwrap().checkout_keys.len(), 1);
    fixture.lock().unwrap().active_subscription = true;
    assert!(matches!(
        start_checkout(&state, &stripe, &auth, Plan::Max).await,
        Err(AppError::Conflict(_))
    ));
    let stored: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT subscription_id,pending_key FROM billing_accounts WHERE owner_id=$1",
    )
    .bind(auth.user_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(stored, ("sub_fixture".into(), None));
    assert_eq!(plan(&state, auth.user_id).await, "free");
    assert_eq!(fixture.lock().unwrap().checkout_keys.len(), 1);
}
