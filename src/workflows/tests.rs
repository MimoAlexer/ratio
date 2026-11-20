use super::*;
use axum::{
    body::Body,
    extract::FromRequest,
    http::{Request, header},
    response::IntoResponse,
};

fn location(r: Redirect) -> String {
    r.into_response().headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string()
}

async fn owner(db: &PgPool, plan: &str) -> Auth {
    let user_id = Uuid::new_v4();
    let email = format!("{user_id}@example.invalid");
    sqlx::query("INSERT INTO users(id,email,plan) VALUES ($1,$2,$3)")
        .bind(user_id)
        .bind(&email)
        .bind(plan)
        .execute(db)
        .await
        .unwrap();
    Auth {
        user_id,
        email,
        csrf: "a".repeat(64),
    }
}

async fn project(db: &PgPool, owner: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO projects(owner_id,name,key) VALUES ($1,'Developer tools','DEV') RETURNING id",
    )
    .bind(owner)
    .fetch_one(db)
    .await
    .unwrap()
}

fn view_form(auth: &Auth, project: Option<Uuid>) -> SaveView {
    SaveView {
        csrf: auth.csrf.clone(),
        name: "Open blockers".into(),
        project: project.map(|p| p.to_string()).unwrap_or_default(),
        filters: Filters {
            status: "ready".into(),
            priority: "high".into(),
            kind: "bug".into(),
            critical: "open".into(),
            scope: "all".into(),
            layout: "board".into(),
            sort_order: "newest".into(),
            grouping: "priority".into(),
            search: "a & b".into(),
        },
    }
}

#[tokio::test]
async fn submitted_view_filters_decode_and_encode_as_a_local_issue_query() {
    let request = Request::post("/workflows/views")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            "csrf=x&name=View&search=C%2B%2B+%26+x%3D1&critical=open",
        ))
        .unwrap();
    let Form(view) = Form::<SaveView>::from_request(request, &()).await.unwrap();
    assert!(view.filters.validate().is_ok());
    let target = view.filters.issue_url(None);
    assert!(target.starts_with("/issues?"));
    let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["q"], "C++ & x=1");
    assert_eq!(pairs["critical"], "open");
    assert_eq!(pairs["scope"], "active");
    assert_eq!(pairs["view"], "list");
    assert_eq!(pairs["sort"], "updated");
    assert_eq!(pairs["group"], "status");
    assert_eq!(pairs.len(), 6);
}

#[sqlx::test]
async fn saved_views_check_scope_ownership_and_effective_tier(db: PgPool) {
    let state = AppState::for_test(db.clone());
    let a = owner(&db, "plus").await;
    let b = owner(&db, "max").await;
    let pa = project(&db, a.user_id).await;
    let pb = project(&db, b.user_id).await;
    let uri =
        format!("/workflows?save=1&project={pa}&q=regression&view=board&critical=open&scope=all&sort=newest&group=priority")
            .parse()
            .unwrap();
    let query = Query::<Pages>::try_from_uri(&uri).unwrap();
    let body = index(State(state.clone()), a.clone(), query)
        .await
        .unwrap()
        .0;
    assert!(body.contains("<details open>"));
    assert!(body.contains("value=\"regression\""));
    assert!(body.contains("value=\"board\" selected"));
    assert!(body.contains("value=\"newest\" selected"));
    assert!(body.contains("value=\"priority\" selected"));
    assert!(body.contains(&format!("value=\"{pa}\" selected")));
    let save = |form| create_view(State(state.clone()), a.clone(), Form(form));
    assert!(matches!(
        save(view_form(&a, None)).await,
        Err(AppError::Forbidden)
    ));
    assert!(matches!(
        save(view_form(&a, Some(pb))).await,
        Err(AppError::NotFound)
    ));
    let mut no_csrf = view_form(&a, Some(pa));
    no_csrf.csrf.clear();
    assert!(matches!(save(no_csrf).await, Err(AppError::Forbidden)));
    assert_eq!(
        location(save(view_form(&a, Some(pa))).await.unwrap()),
        "/workflows#views"
    );
    let id: Uuid = sqlx::query_scalar("SELECT id FROM workflow_views WHERE owner_id=$1")
        .bind(a.user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(matches!(
        open_view(State(state.clone()), b.clone(), Path(id)).await,
        Err(AppError::NotFound)
    ));
    let opened = location(
        open_view(State(state.clone()), a.clone(), Path(id))
            .await
            .unwrap(),
    );
    assert!(opened.contains(&pa.to_string()));
    assert!(opened.contains("sort=newest") && opened.contains("group=priority"));
    let mut board = view_form(&a, Some(pa));
    board.name = "Board without grouping".into();
    board.filters.grouping = "none".into();
    let _ = save(board).await.unwrap();
    let (board_id, grouping): (Uuid, String) = sqlx::query_as(
        "SELECT id,grouping FROM workflow_views WHERE owner_id=$1 AND name='Board without grouping'",
    )
    .bind(a.user_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(grouping, "status");
    let _ = delete_view(
        State(state.clone()),
        a.clone(),
        Path(board_id),
        Form(Confirm {
            csrf: a.csrf.clone(),
        }),
    )
    .await
    .unwrap();
    let _ = create_view(State(state.clone()), b.clone(), Form(view_form(&b, None)))
        .await
        .unwrap();
    let multi: Uuid = sqlx::query_scalar("SELECT id FROM workflow_views WHERE owner_id=$1")
        .bind(b.user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET plan='plus' WHERE id=$1")
        .bind(b.user_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(matches!(
        open_view(State(state.clone()), b.clone(), Path(multi)).await,
        Err(AppError::Forbidden)
    ));
    let del = || {
        Form(Confirm {
            csrf: a.csrf.clone(),
        })
    };
    assert!(matches!(
        delete_view(State(state.clone()), a.clone(), Path(multi), del()).await,
        Err(AppError::NotFound)
    ));
    sqlx::query("UPDATE users SET plan='free' WHERE id=$1")
        .bind(a.user_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(matches!(
        open_view(State(state.clone()), a.clone(), Path(id)).await,
        Err(AppError::Forbidden)
    ));
    assert!(store::storage_used(&db, a.user_id).await.unwrap() > 0);
    let _ = delete_view(State(state), a.clone(), Path(id), del())
        .await
        .unwrap();
    assert_eq!(store::storage_used(&db, a.user_id).await.unwrap(), 0);
}

#[sqlx::test]
async fn template_application_is_a_private_idempotent_native_issue_write(db: PgPool) {
    let state = AppState::for_test(db.clone());
    let a = owner(&db, "plus").await;
    let b = owner(&db, "max").await;
    let pa = project(&db, a.user_id).await;
    let pb = project(&db, b.user_id).await;
    let f = SaveTemplate {
        csrf: a.csrf.clone(),
        name: "Regression".into(),
        draft: TemplateDraft {
            title: "Investigate regression".into(),
            body: "Expected result".into(),
            next_action: "Reproduce".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            kind: "bug".into(),
        },
    };
    let target = location(
        create_template(State(state.clone()), a.clone(), Form(f))
            .await
            .unwrap(),
    );
    let id = Uuid::parse_str(target.strip_prefix("/workflows/templates/").unwrap()).unwrap();
    let command_id = Uuid::new_v4();
    let apply = |auth: &Auth, project| {
        apply_template(
            State(state.clone()),
            auth.clone(),
            Path(id),
            Form(ApplyTemplate {
                csrf: auth.csrf.clone(),
                command_id,
                project,
            }),
        )
    };
    assert!(matches!(apply(&b, pb).await, Err(AppError::NotFound)));
    assert!(matches!(apply(&a, pb).await, Err(AppError::NotFound)));
    sqlx::query("UPDATE projects SET archived=true WHERE id=$1")
        .bind(pa)
        .execute(&db)
        .await
        .unwrap();
    assert!(matches!(apply(&a, pa).await, Err(AppError::Conflict(_))));
    sqlx::query("UPDATE projects SET archived=false WHERE id=$1")
        .bind(pa)
        .execute(&db)
        .await
        .unwrap();
    let (x, y) = tokio::join!(apply(&a, pa), apply(&a, pa));
    let issue_url = location(x.unwrap());
    assert_eq!(issue_url, location(y.unwrap()));
    let issue: Uuid = sqlx::query_scalar(
        "SELECT id FROM issues WHERE owner_id=$1 AND title='Investigate regression'
        AND body='Expected result' AND next_action='Reproduce' AND status='in_progress'
        AND started_at IS NOT NULL AND kind='bug' AND priority='high' AND revision=1",
    )
    .bind(a.user_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(issue_url, format!("/issues/{issue}"));
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM issue_events WHERE owner_id=$1 AND issue_id=$2 AND action='create'",
    )
    .bind(a.user_id)
    .bind(issue)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(events, 1);
    sqlx::query("UPDATE users SET plan='free' WHERE id=$1")
        .bind(a.user_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(matches!(apply(&a, pa).await, Err(AppError::Forbidden)));
    let body = show_template(State(state.clone()), a.clone(), Path(id))
        .await
        .unwrap()
        .0;
    assert!(body.contains("Expected result") && !body.contains("Create issue from template"));
    let _ = delete_template(
        State(state),
        a.clone(),
        Path(id),
        Form(Confirm {
            csrf: a.csrf.clone(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM issues WHERE owner_id=$1")
            .bind(a.user_id)
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
}
