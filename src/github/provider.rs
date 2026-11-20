use crate::{AppError, AppResult};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url, header};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    env,
    time::Duration,
};
use uuid::Uuid;

const API_VERSION: &str = "2026-03-10";
const BODY_LIMIT: usize = 12 * 1024 * 1024;
const PAGE_LIMIT: usize = 1000;

#[derive(Clone)]
pub(super) struct Provider {
    http: Client,
    web: Url,
    api: Url,
    redirect_uri: String,
    client_id: String,
    client_secret: String,
    slug: String,
    token_key: [u8; 32],
    webhook_secret: String,
}

pub(super) struct Token {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub refresh_expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub(super) struct Repo {
    pub id: i64,
    pub installation: i64,
    pub name: String,
    pub(super) can_read_alerts: bool,
    can_read_pulls: bool,
}

pub(super) struct Alert {
    pub number: i64,
    pub package: String,
    pub manifest: String,
    pub patched: Option<String>,
    pub updated_at: DateTime<Utc>,
}

pub(super) struct Pull {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub merged: bool,
}

fn unavailable() -> AppError {
    AppError::Unavailable("GitHub could not complete this check. Try again later.".into())
}

fn token_error() -> AppError {
    AppError::Unavailable("The GitHub connection must be authorized again.".into())
}

fn valid_repo_name(name: &str) -> bool {
    let Some((owner, repo)) = name.split_once('/') else {
        return false;
    };
    [owner, repo].iter().all(|part| {
        !part.is_empty()
            && part.len() <= 100
            && *part != "."
            && *part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    })
}

impl Provider {
    #[cfg(test)]
    pub(super) fn fixture_at(base: &Url) -> Self {
        Provider {
            http: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            web: base.clone(),
            api: base.clone(),
            redirect_uri: "https://workspace.example/integrations/github/callback".to_string(),
            client_id: "fixture-client".into(),
            client_secret: "fixture-secret".into(),
            slug: "workspace".into(),
            token_key: [41; 32],
            webhook_secret: "fixture-signing-secret".into(),
        }
    }

    pub(super) fn configured(http: &Client, base_url: &str) -> Option<Self> {
        let setting = |name| env::var(name).ok().filter(|value| !value.trim().is_empty());
        let slug = setting("WORKSPACE_GITHUB_APP_SLUG")?;
        if !slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return None;
        }
        let token_key = STANDARD
            .decode(setting("WORKSPACE_GITHUB_TOKEN_KEY")?)
            .ok()?
            .try_into()
            .ok()?;
        let redirect_uri = Url::parse(base_url)
            .ok()?
            .join("/integrations/github/callback")
            .ok()?
            .to_string();
        Some(Self {
            http: http.clone(),
            web: Url::parse("https://github.com/").ok()?,
            api: Url::parse("https://api.github.com/").ok()?,
            redirect_uri,
            client_id: setting("WORKSPACE_GITHUB_CLIENT_ID")?,
            client_secret: setting("WORKSPACE_GITHUB_CLIENT_SECRET")?,
            slug,
            token_key,
            webhook_secret: setting("WORKSPACE_GITHUB_WEBHOOK_SECRET")?,
        })
    }

    fn verifier(&self, state: &str) -> String {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(&self.token_key).expect("fixed length key");
        mac.update(b"workspace.github.oauth.pkce.v1\0");
        mac.update(state.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    pub(super) fn authorize_url(&self, state: &str) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier(state)));
        let mut url = self.web.join("login/oauth/authorize").expect("static path");
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("state", state)
            .append_pair("scope", "offline_access")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        url.into()
    }

    pub(super) fn install_url(&self) -> String {
        self.web
            .join(&format!("apps/{}/installations/new", self.slug))
            .expect("validated app slug")
            .into()
    }

    pub(super) fn seal(&self, owner: Uuid, clear: &str) -> AppResult<Vec<u8>> {
        let mut nonce = [0u8; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| unavailable())?;
        let mut sealed = vec![1];
        sealed.extend_from_slice(&nonce);
        let cipher = XChaCha20Poly1305::new((&self.token_key).into());
        sealed.extend(
            cipher
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: clear.as_bytes(),
                        aad: owner.as_bytes(),
                    },
                )
                .map_err(|_| token_error())?,
        );
        Ok(sealed)
    }

    pub(super) fn unseal(&self, owner: Uuid, sealed: &[u8]) -> AppResult<String> {
        if sealed.first() != Some(&1) || sealed.len() < 41 {
            return Err(token_error());
        }
        let cipher = XChaCha20Poly1305::new((&self.token_key).into());
        let clear = cipher
            .decrypt(
                XNonce::from_slice(&sealed[1..25]),
                Payload {
                    msg: &sealed[25..],
                    aad: owner.as_bytes(),
                },
            )
            .map_err(|_| token_error())?;
        String::from_utf8(clear).map_err(|_| token_error())
    }

    pub(super) fn valid_webhook(&self, raw: &[u8], signature: &str) -> bool {
        let Some(hex_digest) = signature.strip_prefix("sha256=") else {
            return false;
        };
        if hex_digest.len() != 64 {
            return false;
        }
        let Ok(digest) = hex::decode(hex_digest) else {
            return false;
        };
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(self.webhook_secret.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(raw);
        mac.verify_slice(&digest).is_ok()
    }

    pub(super) async fn exchange(&self, code: &str, state: &str) -> AppResult<Token> {
        let verifier = self.verifier(state);
        self.token(&[
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("code_verifier", &verifier),
        ])
        .await
    }

    pub(super) async fn refresh(&self, refresh: &str) -> AppResult<Token> {
        self.token(&[("grant_type", "refresh_token"), ("refresh_token", refresh)])
            .await
    }

    async fn token(&self, fields: &[(&str, &str)]) -> AppResult<Token> {
        #[derive(Deserialize)]
        struct TokenBody {
            access_token: Option<String>,
            refresh_token: Option<String>,
            token_type: Option<String>,
            expires_in: Option<i64>,
            refresh_token_expires_in: Option<i64>,
            error: Option<String>,
        }
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
        ];
        form.extend_from_slice(fields);
        let url = self
            .web
            .join("login/oauth/access_token")
            .map_err(|_| unavailable())?;
        let request = self
            .http
            .post(url)
            .header(header::ACCEPT, "application/json")
            .form(&form);
        let response = send(request).await?;
        let data: TokenBody = read_json(response).await?;
        if data.error.is_some()
            || !data
                .token_type
                .is_some_and(|ty| ty.eq_ignore_ascii_case("bearer"))
        {
            return Err(AppError::Forbidden);
        }
        let expires = |seconds: Option<i64>| -> AppResult<Option<DateTime<Utc>>> {
            seconds
                .map(|n| {
                    if n <= 0 {
                        return Err(token_error());
                    }
                    TimeDelta::try_seconds(n)
                        .and_then(|d| Utc::now().checked_add_signed(d))
                        .ok_or_else(token_error)
                })
                .transpose()
        };
        Ok(Token {
            access: data
                .access_token
                .filter(|s| !s.is_empty())
                .ok_or_else(token_error)?,
            refresh: data.refresh_token.filter(|s| !s.is_empty()),
            expires_at: expires(data.expires_in)?,
            refresh_expires_at: expires(data.refresh_token_expires_in)?,
        })
    }

    fn api_url(&self, path: &str) -> AppResult<Url> {
        self.api.join(path).map_err(|_| unavailable())
    }

    async fn get(&self, access: &str, url: Url) -> AppResult<Response> {
        send(
            self.http
                .get(url)
                .bearer_auth(access)
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION),
        )
        .await
    }

    pub(super) async fn identity(&self, access: &str) -> AppResult<i64> {
        #[derive(Deserialize)]
        struct Identity {
            id: i64,
        }
        let user: Identity = read_json(self.get(access, self.api_url("user")?).await?).await?;
        if user.id <= 0 {
            return Err(unavailable());
        }
        Ok(user.id)
    }

    async fn pages<T: DeserializeOwned, R>(
        &self,
        access: &str,
        initial: Url,
        alias: Option<&str>,
        mut unpack: impl FnMut(T) -> AppResult<Vec<R>>,
    ) -> AppResult<Vec<R>> {
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        let mut next = Some(initial.clone());
        while let Some(url) = next.take() {
            if seen.len() >= PAGE_LIMIT || !seen.insert(url.to_string()) {
                return Err(unavailable());
            }
            let response = self.get(access, url).await?;
            next = next_page(&initial, alias, response.headers())?;
            items.extend(unpack(read_json(response).await?)?);
        }
        Ok(items)
    }

    pub(super) async fn repositories(&self, access: &str) -> AppResult<Vec<Repo>> {
        self.list_repos(access, None).await
    }

    pub(super) async fn repository(
        &self,
        access: &str,
        installation: i64,
        repo: i64,
    ) -> AppResult<Option<Repo>> {
        Ok(self
            .list_repos(access, Some(installation))
            .await?
            .into_iter()
            .find(|r| r.id == repo))
    }

    async fn list_repos(
        &self,
        access: &str,
        requested_installation: Option<i64>,
    ) -> AppResult<Vec<Repo>> {
        #[derive(Deserialize)]
        struct InstallPage {
            installations: Vec<Installation>,
        }
        #[derive(Deserialize)]
        struct Installation {
            id: i64,
            app_slug: String,
            permissions: BTreeMap<String, String>,
            suspended_at: Option<String>,
        }
        #[derive(Deserialize)]
        struct RepoPage {
            repositories: Vec<RepoBody>,
        }
        #[derive(Deserialize)]
        struct RepoBody {
            id: i64,
            full_name: String,
        }
        let entries = self
            .pages(
                access,
                self.api_url("user/installations?per_page=100")?,
                None,
                |p: InstallPage| Ok(p.installations),
            )
            .await?;
        let mut repos = Vec::new();
        for installation in entries {
            // Each endpoint still checks the *user's* effective permission separately.
            if requested_installation.is_some_and(|id| id != installation.id)
                || installation.suspended_at.is_some()
                || installation.app_slug != self.slug
                || installation
                    .permissions
                    .values()
                    .any(|v| !matches!(v.as_str(), "none" | "read"))
            {
                continue;
            }
            if installation.id <= 0 {
                return Err(unavailable());
            }
            let path = format!(
                "user/installations/{}/repositories?per_page=100",
                installation.id
            );
            let visible = self
                .pages(access, self.api_url(&path)?, None, |p: RepoPage| {
                    Ok(p.repositories)
                })
                .await?;
            for repo in visible {
                if repo.id <= 0 || !valid_repo_name(&repo.full_name) {
                    return Err(unavailable());
                }
                repos.push(Repo {
                    id: repo.id,
                    installation: installation.id,
                    name: repo.full_name,
                    can_read_alerts: installation
                        .permissions
                        .get("vulnerability_alerts")
                        .is_some_and(|p| p == "read"),
                    can_read_pulls: installation
                        .permissions
                        .get("pull_requests")
                        .is_some_and(|p| p == "read"),
                });
            }
        }
        Ok(repos)
    }

    fn repo_url(&self, repo: &Repo, subpath: &str) -> AppResult<Url> {
        if repo.id <= 0 || !valid_repo_name(&repo.name) {
            return Err(AppError::Forbidden);
        }
        self.api_url(&format!("repos/{}/{}", repo.name, subpath))
    }

    pub(super) async fn alert_access(&self, access: &str, repo: &Repo) -> AppResult<()> {
        if !repo.can_read_alerts {
            return Err(AppError::Forbidden);
        }
        let url = self.repo_url(
            repo,
            "dependabot/alerts?state=open&severity=critical&per_page=1",
        )?;
        let _: Vec<serde::de::IgnoredAny> = read_json(self.get(access, url).await?).await?;
        Ok(())
    }

    pub(super) async fn alerts(&self, access: &str, repo: &Repo) -> AppResult<Vec<Alert>> {
        if !repo.can_read_alerts {
            return Err(AppError::Forbidden);
        }
        #[derive(Deserialize)]
        struct RawAlert {
            number: i64,
            state: String,
            dependency: Dependency,
            security_vulnerability: Vulnerability,
            updated_at: DateTime<Utc>,
        }
        #[derive(Deserialize)]
        struct Dependency {
            package: Package,
            manifest_path: String,
        }
        #[derive(Deserialize)]
        struct Package {
            name: String,
        }
        #[derive(Deserialize)]
        struct Vulnerability {
            severity: String,
            first_patched_version: Option<Version>,
        }
        #[derive(Deserialize)]
        struct Version {
            identifier: String,
        }
        let url = self.repo_url(
            repo,
            "dependabot/alerts?state=open&severity=critical&per_page=100",
        )?;
        let numeric_path = format!("/repositories/{}/dependabot/alerts", repo.id);
        self.pages(access, url, Some(&numeric_path), |page: Vec<RawAlert>| {
            page.into_iter()
                .filter(|a| a.state == "open" && a.security_vulnerability.severity == "critical")
                .map(|a| {
                    if a.number <= 0 || a.dependency.package.name.is_empty() {
                        return Err(unavailable());
                    }
                    Ok(Alert {
                        number: a.number,
                        package: a.dependency.package.name,
                        manifest: a.dependency.manifest_path,
                        patched: a
                            .security_vulnerability
                            .first_patched_version
                            .map(|p| p.identifier),
                        updated_at: a.updated_at,
                    })
                })
                .collect()
        })
        .await
    }

    pub(super) async fn pull(&self, access: &str, repo: &Repo, n: i64) -> AppResult<Pull> {
        if !repo.can_read_pulls {
            return Err(AppError::Forbidden);
        }
        #[derive(Deserialize)]
        struct RawPull {
            number: i64,
            title: String,
            body: Option<String>,
            state: String,
            merged: bool,
            base: Base,
        }
        #[derive(Deserialize)]
        struct Base {
            repo: BaseRepo,
        }
        #[derive(Deserialize)]
        struct BaseRepo {
            id: i64,
        }
        if n <= 0 {
            return Err(AppError::BadRequest(
                "Enter a valid pull request number.".into(),
            ));
        }
        let data: RawPull = read_json(
            self.get(access, self.repo_url(repo, &format!("pulls/{n}"))?)
                .await?,
        )
        .await?;
        if data.number != n || data.base.repo.id != repo.id {
            return Err(AppError::Forbidden);
        }
        Ok(Pull {
            number: n,
            title: data.title,
            body: data.body,
            state: data.state,
            merged: data.merged,
        })
    }
}

async fn send(request: RequestBuilder) -> AppResult<Response> {
    let response = request
        .header(header::USER_AGENT, "Workspace-GitHub-App")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| unavailable())?;
    match response.status() {
        StatusCode::OK => Ok(response),
        StatusCode::FORBIDDEN
            if response.headers().contains_key(header::RETRY_AFTER)
                || response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0") =>
        {
            Err(unavailable())
        }
        StatusCode::FORBIDDEN => {
            // A secondary rate limit can have no Retry-After or exhausted primary budget.
            let message = read_json::<serde_json::Value>(response)
                .await
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_ascii_lowercase))
                .unwrap_or_default();
            if message.contains("rate limit") {
                Err(unavailable())
            } else {
                Err(AppError::Forbidden)
            }
        }
        StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND | StatusCode::GONE => {
            Err(AppError::Forbidden)
        }
        _ => Err(unavailable()),
    }
}

async fn read_json<T: DeserializeOwned>(mut response: Response) -> AppResult<T> {
    if response
        .content_length()
        .is_some_and(|len| len > BODY_LIMIT as u64)
    {
        return Err(unavailable());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if chunk.len() > BODY_LIMIT - body.len() {
            return Err(unavailable());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| unavailable())
}

fn next_page(
    initial: &Url,
    alias: Option<&str>,
    headers: &header::HeaderMap,
) -> AppResult<Option<Url>> {
    let mut next = None;
    for value in headers.get_all(header::LINK) {
        for link in value.to_str().map_err(|_| unavailable())?.split(',') {
            let (target, params) = link.trim().split_once('>').ok_or_else(unavailable)?;
            let target = target.strip_prefix('<').ok_or_else(unavailable)?;
            let relation = params
                .split(';')
                .map(str::trim)
                .find_map(|p| p.strip_prefix("rel="))
                .ok_or_else(unavailable)?;
            if !relation
                .trim_matches('"')
                .split_ascii_whitespace()
                .any(|v| v == "next")
            {
                continue;
            }
            let url = Url::parse(target).map_err(|_| unavailable())?;
            if next.is_some()
                || url.origin() != initial.origin()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || (url.path() != initial.path() && Some(url.path()) != alias)
            {
                return Err(unavailable());
            }
            next = Some(url);
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::Request,
        http::Response as HttpResponse,
    };
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tokio::{net::TcpListener, task::JoinHandle};

    struct Fixture {
        api: Provider,
        server: JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    struct Received {
        method: String,
        path: String,
        query: String,
        bearer: Option<String>,
        body: String,
    }
    async fn fixture(
        handler: impl Fn(Received, &Url) -> HttpResponse<Body> + Send + Sync + 'static,
    ) -> Fixture {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let api = Provider::fixture_at(&base);
        let handler = Arc::new(handler);
        let app = Router::new().fallback(move |req: Request| {
            let handler = handler.clone();
            let base = base.clone();
            async move {
                let (parts, body) = req.into_parts();
                let received = Received {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_owned(),
                    query: parts.uri.query().unwrap_or_default().to_owned(),
                    bearer: parts
                        .headers
                        .get(header::AUTHORIZATION)
                        .map(|v| v.to_str().unwrap().to_owned()),
                    body: String::from_utf8(to_bytes(body, 16 * 1024).await.unwrap().into())
                        .unwrap(),
                };
                handler(received, &base)
            }
        });
        Fixture {
            api,
            server: tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        }
    }
    fn response(status: u16, value: Value) -> HttpResponse<Body> {
        HttpResponse::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    }
    fn repo() -> Repo {
        Repo {
            id: 12,
            installation: 7,
            name: "Octo/MyRepo".into(),
            can_read_alerts: true,
            can_read_pulls: true,
        }
    }
    fn alert(n: i64, severity: &str) -> Value {
        json!({"number":n,"state":"open","dependency":{"package":{"name":"fixture-lib"},"manifest_path":"Cargo.lock"},
            "security_vulnerability":{"severity":severity,"first_patched_version":{"identifier":"2.1"}},
            "updated_at":"2026-11-20T10:00:00Z"})
    }

    #[test]
    fn credentials_are_owner_bound_and_tamper_evident() {
        let p = Provider::fixture_at(&Url::parse("http://localhost/").unwrap());
        let owner = Uuid::new_v4();
        let encrypted = p.seal(owner, "a test credential").unwrap();
        assert!(!encrypted.windows(17).any(|w| w == b"a test credential"));
        assert_eq!(p.unseal(owner, &encrypted).unwrap(), "a test credential");
        assert_ne!(encrypted, p.seal(owner, "a test credential").unwrap());
        assert!(p.unseal(Uuid::new_v4(), &encrypted).is_err());
        let mut changed = encrypted;
        *changed.last_mut().unwrap() ^= 0x04;
        assert!(p.unseal(owner, &changed).is_err());
        assert!(p.unseal(owner, &[1; 6]).is_err());
    }

    #[tokio::test]
    async fn oauth_uses_pkce_and_checks_the_credential_type() {
        let f = fixture(|req,_| {
            assert_eq!(req.method,"POST");
            assert_eq!(req.path,"/login/oauth/access_token");
            assert!(req.query.is_empty());
            let fields=Url::parse(&format!("http://localhost/?{}", req.body)).unwrap();
            let fields:BTreeMap<_,_>=fields.query_pairs().collect();
            assert_eq!(fields["client_secret"],"fixture-secret");
            if fields.contains_key("code") {
                assert_eq!(fields["code"],"fixture-code");
                assert_eq!(fields["code_verifier"].len(),43);
                response(200,json!({"access_token":"a1","token_type":"bearer","expires_in":28800,"refresh_token":"r1","refresh_token_expires_in":15552000}))
            } else {
                assert_eq!(fields["grant_type"],"refresh_token");
                assert_eq!(fields["refresh_token"],"r1");
                response(200,json!({"error":"bad_refresh_token"}))
            }
        }).await;
        let auth = Url::parse(&f.api.authorize_url("opaque-state")).unwrap();
        let query: BTreeMap<_, _> = auth.query_pairs().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(f.api.verifier("opaque-state")))
        );
        assert!(!query.contains_key("code_verifier"));
        let issued = f
            .api
            .exchange("fixture-code", "opaque-state")
            .await
            .unwrap();
        assert_eq!(issued.access, "a1");
        assert!(issued.expires_at.unwrap() > Utc::now());
        assert!(issued.refresh_expires_at.unwrap() > issued.expires_at.unwrap());
        assert!(matches!(
            f.api.refresh(issued.refresh.as_deref().unwrap()).await,
            Err(AppError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn repositories_follow_owner_grants_and_skip_suspensions_and_write_permissions() {
        let f=fixture(|req,base| {
            assert_eq!(req.bearer.as_deref(),Some("Bearer user-token"));
            match req.path.as_str() {
                "/user" => response(200,json!({"id":80})),
                "/user/installations" => response(200,json!({"installations":[
                    {"id":7,"app_slug":"workspace","permissions":{"metadata":"read","pull_requests":"read","vulnerability_alerts":"read"}},
                    {"id":8,"app_slug":"workspace","permissions":{"metadata":"read","issues":"write"}},
                    {"id":9,"app_slug":"workspace","permissions":{},"suspended_at":"2026-11-19T12:55:00Z"},
                    {"id":10,"app_slug":"another-app","permissions":{"metadata":"read"}}
                ]})),
                "/user/installations/7/repositories" if req.query.contains("page=2") =>
                    response(200,json!({"repositories":[{"id":13,"full_name":"Octo/Second"}]})),
                "/user/installations/7/repositories" => {
                    let mut r=response(200,json!({"repositories":[{"id":12,"full_name":"Octo/MyRepo"}]}));
                    let next=base.join("user/installations/7/repositories?page=2").unwrap();
                    r.headers_mut().insert(header::LINK,format!("<{next}>; rel=\"next\"").parse().unwrap());
                    r
                }
                _ => response(500,json!({})),
            }
        }).await;
        assert_eq!(f.api.identity("user-token").await.unwrap(), 80);
        let repos = f.api.repositories("user-token").await.unwrap();
        assert_eq!(
            repos
                .iter()
                .map(|r| (r.id, r.installation, r.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(12, 7, "Octo/MyRepo"), (13, 7, "Octo/Second")]
        );
    }

    #[tokio::test]
    async fn selected_repository_queries_only_its_installation() {
        let f = fixture(|req, _| match req.path.as_str() {
            "/user/installations" => response(200, json!({"installations":[
                {"id":11,"app_slug":"workspace","permissions":{"metadata":"read"}},
                {"id":7,"app_slug":"workspace","permissions":{"metadata":"read","vulnerability_alerts":"read"}}
            ]})),
            "/user/installations/7/repositories" => response(200, json!({"repositories":[
                {"id":12,"full_name":"Octo/MyRepo"}, {"id":13,"full_name":"Octo/Second"}
            ]})),
            // Requesting the unrelated installation would fail the lookup.
            _ => response(500, json!({})),
        }).await;
        let selected = f
            .api
            .repository("user-token", 7, 13)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (selected.id, selected.installation, selected.name.as_str()),
            (13, 7, "Octo/Second")
        );
        assert!(
            f.api
                .repository("user-token", 7, 999)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            f.api
                .repository("user-token", 999, 13)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn critical_alerts_follow_numeric_continuations_and_filter_upstream_rows() {
        let f = fixture(|req, base| {
            assert_eq!(req.bearer.as_deref(), Some("Bearer user-token"));
            if req.path == "/repositories/12/dependabot/alerts" {
                return response(200, json!([alert(9, "critical"), alert(10, "high")]));
            }
            assert_eq!(req.path, "/repos/Octo/MyRepo/dependabot/alerts");
            assert!(req.query.contains("state=open") && req.query.contains("severity=critical"));
            let mut r = response(200, json!([alert(7, "critical")]));
            let next = base
                .join("repositories/12/dependabot/alerts?after=abc")
                .unwrap();
            r.headers_mut().insert(
                header::LINK,
                format!("<{next}>; rel=\"next\"").parse().unwrap(),
            );
            r
        })
        .await;
        let alerts = f.api.alerts("user-token", &repo()).await.unwrap();
        assert_eq!(
            alerts.iter().map(|a| a.number).collect::<Vec<_>>(),
            vec![7, 9]
        );
        assert_eq!(alerts[0].manifest, "Cargo.lock");
        assert_eq!(alerts[0].package, "fixture-lib");
        assert_eq!(alerts[0].patched.as_deref(), Some("2.1"));
        assert_eq!(alerts[0].updated_at.timestamp(), 1795168800);
    }

    #[tokio::test]
    async fn second_page_failure_discards_first_page() {
        let f = fixture(|req, base| {
            if req.query.contains("after=") {
                return response(503, json!({"message":"service unavailable"}));
            }
            let mut r = response(200, json!([alert(7, "critical")]));
            let next = base
                .join("repos/Octo/MyRepo/dependabot/alerts?after=abc")
                .unwrap();
            r.headers_mut().insert(
                header::LINK,
                format!("<{next}>; rel=\"next\"").parse().unwrap(),
            );
            r
        })
        .await;
        assert!(matches!(
            f.api.alerts("user-token", &repo()).await,
            Err(AppError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn pr_read_survives_loss_of_alert_access() {
        let f=fixture(|req,_| {
            assert_eq!(req.bearer.as_deref(),Some("Bearer user-token"));
            match req.path.as_str() {
                "/repos/Octo/MyRepo/pulls/42"=>response(200,json!({"number":42,"title":"Fix parser","state":"closed","body":"Fixes ABC-7","merged":true,"base":{"repo":{"id":12}}})),
                "/repos/Octo/MyRepo/pulls/43"=>response(200,json!({"number":43,"title":"Other","state":"open","merged":false,"base":{"repo":{"id":99}}})),
                "/repos/Octo/MyRepo/dependabot/alerts"=>response(403,json!({"message":"Resource not accessible by integration"})),
                _=>response(500,json!({})),
            }
        }).await;
        assert!(matches!(
            f.api.alert_access("user-token", &repo()).await,
            Err(AppError::Forbidden)
        ));
        let pr = f.api.pull("user-token", &repo(), 42).await.unwrap();
        assert_eq!(
            (pr.title.as_str(), pr.number, pr.state.as_str(), pr.merged),
            ("Fix parser", 42, "closed", true)
        );
        assert_eq!(pr.body.as_deref(), Some("Fixes ABC-7"));
        assert!(matches!(
            f.api.pull("user-token", &repo(), 43).await,
            Err(AppError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn reports_rate_limits_and_partial_http_responses_as_unavailable() {
        let f = fixture(|req, _| {
            if req.path.ends_with("/pulls/1") {
                return response(
                    403,
                    json!({"message":"You have exceeded a secondary rate limit."}),
                );
            }
            response(206, json!([]))
        })
        .await;
        assert!(matches!(
            f.api.pull("user-token", &repo(), 1).await,
            Err(AppError::Unavailable(_))
        ));
        assert!(matches!(
            f.api.alerts("user-token", &repo()).await,
            Err(AppError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn repository_visibility_does_not_authorize_a_missing_app_capability() {
        let f = fixture(|_, _| response(500, json!({}))).await;
        let mut visible = repo();
        visible.can_read_alerts = false;
        visible.can_read_pulls = false;
        assert!(matches!(
            f.api.alert_access("user-token", &visible).await,
            Err(AppError::Forbidden)
        ));
        assert!(matches!(
            f.api.alerts("user-token", &visible).await,
            Err(AppError::Forbidden)
        ));
        assert!(matches!(
            f.api.pull("user-token", &visible, 1).await,
            Err(AppError::Forbidden)
        ));
    }

    #[test]
    fn rejects_third_party_or_wrong_endpoint_pagination() {
        let initial = Url::parse("https://api.github.com/repos/a/b/dependabot/alerts").unwrap();
        for url in [
            "https://other.example/repos/a/b/dependabot/alerts",
            "https://api.github.com/user",
            "https://api.github.com/repositories/13/dependabot/alerts",
        ] {
            let mut headers = header::HeaderMap::new();
            headers.insert(
                header::LINK,
                format!("<{url}>; rel=\"next\"").parse().unwrap(),
            );
            assert!(
                next_page(
                    &initial,
                    Some("/repositories/12/dependabot/alerts"),
                    &headers
                )
                .is_err()
            );
        }
    }

    #[test]
    fn verifies_raw_webhook_signature() {
        let p = Provider::fixture_at(&Url::parse("http://localhost/").unwrap());
        let body = b"{ \"action\": \"deleted\" }";
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(b"fixture-signing-secret").unwrap();
        mac.update(body);
        let signed = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(p.valid_webhook(body, &signed));
        assert!(!p.valid_webhook(b"{}", &signed));
        assert!(!p.valid_webhook(body, "sha1=0"));
    }
}
