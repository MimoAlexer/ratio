use crate::{AppError, AppResult, AppState};
use axum::http::{HeaderMap, header};
use reqwest::Url;
use std::{
    env,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

pub(super) fn dev_links(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>) -> bool {
    !state.config.production
        && env::var("DEV_LOGIN_LINKS").as_deref() == Ok("1")
        && direct_loopback(&state.config.base_url, headers, peer)
}

fn direct_loopback(base_url: &str, headers: &HeaderMap, peer: Option<SocketAddr>) -> bool {
    if !peer.is_some_and(|a| a.ip().is_loopback())
        || [
            "forwarded",
            "via",
            "x-real-ip",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "cf-connecting-ip",
        ]
        .iter()
        .any(|h| headers.contains_key(*h))
    {
        return false;
    }
    let Ok(base) = Url::parse(base_url) else {
        return false;
    };
    let Some(host) = base.host_str() else {
        return false;
    };
    if base.scheme() != "http"
        || !(host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()))
    {
        return false;
    }
    let authority = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::uri::Authority>().ok());
    authority
        .and_then(|h| Url::parse(&format!("http://{h}/")).ok())
        .is_some_and(|request| {
            request.origin() == base.origin()
                && request.username().is_empty()
                && request.password().is_none()
        })
}

pub(super) async fn mail_link(state: &AppState, email: &str, url: &str) -> AppResult<()> {
    let api_key = env::var("RESEND_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(mail_unavailable)?;
    let sender = env::var("MAIL_FROM")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(mail_unavailable)?;
    send_verification_email(
        &state.http,
        "https://api.resend.com/emails",
        &api_key,
        &sender,
        email,
        url,
    )
    .await
}

fn mail_unavailable() -> AppError {
    AppError::Unavailable("Email delivery is unavailable. Please try again later.".into())
}

async fn send_verification_email(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    sender: &str,
    email: &str,
    url: &str,
) -> AppResult<()> {
    let message = format!(
        "Open this link to continue in Working Set. It expires in 15 minutes and can be used once.\n\n{url}\n\nIf you did not request this email, you can ignore it."
    );
    let response = client.post(endpoint)
        .timeout(Duration::from_secs(12))
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "from": sender, "to": [email], "subject": "Your Working Set verification link", "text": message
        }))
        .send().await.map_err(|_| mail_unavailable())?;
    if !response.status().is_success() {
        return Err(mail_unavailable());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, routing::post};
    use serde_json::Value;

    #[test]
    fn local_preview_requires_an_exact_host_and_direct_loopback_connection() {
        let base = "http://127.0.0.1:3479";
        let peer = Some("127.0.0.1:44888".parse().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:3479".parse().unwrap());
        assert!(direct_loopback(base, &headers, peer));
        assert!(!direct_loopback(base, &headers, None));
        assert!(!direct_loopback(
            base,
            &headers,
            Some("192.0.2.7:44888".parse().unwrap())
        ));
        assert!(!direct_loopback("http://example.org:3479", &headers, peer));
        assert!(!direct_loopback("http://127.0.0.1:3480", &headers, peer));
        headers.insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
        assert!(!direct_loopback(base, &headers, peer));
        headers.remove("x-forwarded-for");
        headers.insert(
            header::HOST,
            "other.example@127.0.0.1:3479".parse().unwrap(),
        );
        assert!(!direct_loopback(base, &headers, peer));
    }

    #[tokio::test]
    async fn transactional_email_fixture_accepts_the_request_and_reports_provider_failure() {
        let server = Router::new()
            .route(
                "/emails",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Bearer fixture-value");
                    assert_eq!(body["from"], "sender@example.invalid");
                    assert_eq!(body["to"], serde_json::json!(["reader@example.invalid"]));
                    assert!(
                        body["text"]
                            .as_str()
                            .unwrap()
                            .contains("https://workspace.example.invalid/verify")
                    );
                    StatusCode::CREATED
                }),
            )
            .route(
                "/unavailable",
                post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(socket, server).await.unwrap() });
        let client = reqwest::Client::new();
        for (path, success) in [("emails", true), ("unavailable", false)] {
            let result = send_verification_email(
                &client,
                &format!("http://{address}/{path}"),
                "fixture-value",
                "sender@example.invalid",
                "reader@example.invalid",
                "https://workspace.example.invalid/verify",
            )
            .await;
            assert_eq!(result.is_ok(), success)
        }
        task.abort();
    }
}
