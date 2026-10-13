//! Authenticated fetch for the "hosts API" source type.
//!
//! The counterpart service sits behind an Authelia-style `auth_request`:
//! an unauthenticated request comes back either as a bare 401, or (once
//! an nginx `auth_request` integration is in front of it) as a 3xx whose
//! `Location` header points at the login portal — either way we discover
//! the portal from that response instead of hardcoding it anywhere, POST
//! credentials there, and retry with the resulting session cookie.
//!
//! This repo is public: nothing here ever names a real endpoint,
//! company, or credential. The base URL and username are user-supplied
//! (`AppConfig::hosts_api_base_url` / `hosts_api_username`); the
//! password lives only in the OS keychain (`KEYRING_SERVICE`), never in
//! the manifest, `config.json`, or any file this app writes.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::storage::AppState;

pub const KEYRING_SERVICE: &str = "switchhosts-hosts-api";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Machine {
    pub label: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub public_ip: Option<String>,
    #[serde(default)]
    pub hosts_file: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MachinesResponse {
    machines: Vec<Machine>,
}

#[derive(Debug)]
pub enum HostsApiError {
    InvalidUrl,
    Network(String),
    LoginFailed(String),
    NoLoginRedirect(u16),
    BadResponse(String),
}

impl std::fmt::Display for HostsApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl => write!(f, "invalid hosts API base URL"),
            Self::Network(message) => write!(f, "{message}"),
            Self::LoginFailed(message) => write!(f, "login failed: {message}"),
            Self::NoLoginRedirect(status) => {
                write!(f, "request failed: HTTP {status} (no login redirect to follow)")
            }
            Self::BadResponse(message) => write!(f, "bad response from server: {message}"),
        }
    }
}

/// Mirrors `http::build_client`, but with redirects disabled -- we need
/// to inspect a 3xx's `Location` ourselves to find the login portal
/// rather than have it silently followed, and no cookie jar, since we
/// pass the session cookie through by hand (see `authenticated_get`).
pub fn build_client(state: &AppState) -> Result<reqwest::Client, HostsApiError> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy_url) = crate::http::configured_proxy_url_from_state(state) {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|_| HostsApiError::Network("invalid proxy settings".into()))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| HostsApiError::Network(e.to_string()))
}

#[derive(Serialize)]
struct FirstFactor<'a> {
    username: &'a str,
    password: &'a str,
    #[serde(rename = "keepMeLoggedIn")]
    keep_me_logged_in: bool,
}

/// GETs `url`, optionally sending `cookie` as the `Cookie` header.
/// Returns (status, body, `Location` header if present).
async fn fetch_once(
    client: &reqwest::Client,
    url: &str,
    cookie: Option<&str>,
) -> Result<(u16, String, Option<String>), HostsApiError> {
    let mut request = client.get(url);
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    let response = request
        .send()
        .await
        .map_err(|e| HostsApiError::Network(crate::http::request_error_message(e)))?;
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response
        .text()
        .await
        .map_err(|e| HostsApiError::Network(crate::http::request_error_message(e)))?;
    Ok((status, body, location))
}

fn login_portal_origin(location: &str) -> Option<String> {
    let url = reqwest::Url::parse(location).ok()?;
    url.host_str()?;
    Some(url.origin().ascii_serialization())
}

/// First `name=value` pair out of a `Set-Cookie` header, dropping
/// attributes (`Path=`, `HttpOnly`, etc).
fn session_cookie_from_set_cookie(set_cookie: &str) -> Option<String> {
    let pair = set_cookie.split(';').next()?.trim();
    if pair.is_empty() || !pair.contains('=') {
        return None;
    }
    Some(pair.to_string())
}

async fn login(
    client: &reqwest::Client,
    portal_origin: &str,
    username: &str,
    password: &str,
) -> Result<String, HostsApiError> {
    let url = format!("{portal_origin}/api/firstfactor");
    let response = client
        .post(&url)
        .json(&FirstFactor {
            username,
            password,
            keep_me_logged_in: true,
        })
        .send()
        .await
        .map_err(|e| HostsApiError::Network(crate::http::request_error_message(e)))?;
    let status = response.status();
    let set_cookie = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .find_map(|v| v.to_str().ok().and_then(session_cookie_from_set_cookie));
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(HostsApiError::LoginFailed(format!("HTTP {status} {body}")));
    }
    set_cookie.ok_or_else(|| HostsApiError::LoginFailed("server did not set a session cookie".into()))
}

/// Probes `url`; if it comes back unauthenticated (bare 401, or a 3xx
/// carrying a `Location` to the login portal), logs in and retries once.
pub async fn authenticated_get(
    client: &reqwest::Client,
    url: &str,
    username: &str,
    password: &str,
) -> Result<String, HostsApiError> {
    let (status, body, location) = fetch_once(client, url, None).await?;
    if (200..300).contains(&status) {
        return Ok(body);
    }

    let portal_origin = location
        .as_deref()
        .and_then(login_portal_origin)
        .ok_or(HostsApiError::NoLoginRedirect(status))?;

    let cookie = login(client, &portal_origin, username, password).await?;

    let (status, body, _) = fetch_once(client, url, Some(&cookie)).await?;
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(HostsApiError::BadResponse(format!("HTTP {status}")))
    }
}

fn normalize_base_url(base_url: &str) -> Result<String, HostsApiError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() || reqwest::Url::parse(trimmed).is_err() {
        return Err(HostsApiError::InvalidUrl);
    }
    Ok(trimmed.to_string())
}

pub async fn fetch_machines(
    client: &reqwest::Client,
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<Vec<Machine>, HostsApiError> {
    let base_url = normalize_base_url(base_url)?;
    let url = format!("{base_url}/api/machines");
    let body = authenticated_get(client, &url, username, password).await?;
    let parsed: MachinesResponse =
        serde_json::from_str(&body).map_err(|e| HostsApiError::BadResponse(e.to_string()))?;
    Ok(parsed.machines)
}

/// The company's always-added fixed block, fetched alongside the machine
/// list so it stays correct no matter which machine (if any) is on.
pub async fn fetch_company_block(
    client: &reqwest::Client,
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<String, HostsApiError> {
    let base_url = normalize_base_url(base_url)?;
    let url = format!("{base_url}/api/company-hosts");
    authenticated_get(client, &url, username, password).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::Router;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn login_portal_origin_extracts_scheme_and_authority() {
        let loc = "https://auth.example.test/?rd=https%3A%2F%2Fhosts.example.test%2Fapi%2Fmachines";
        assert_eq!(login_portal_origin(loc).as_deref(), Some("https://auth.example.test"));
    }

    #[test]
    fn login_portal_origin_keeps_nonstandard_port() {
        assert_eq!(
            login_portal_origin("http://auth.example.test:8080/login").as_deref(),
            Some("http://auth.example.test:8080")
        );
    }

    #[test]
    fn login_portal_origin_rejects_relative_location() {
        assert_eq!(login_portal_origin("/login?rd=%2Fapi%2Fmachines"), None);
    }

    #[test]
    fn session_cookie_drops_attributes() {
        assert_eq!(
            session_cookie_from_set_cookie("authelia_session=abc123; Path=/; HttpOnly; Secure"),
            Some("authelia_session=abc123".to_string())
        );
    }

    #[test]
    fn session_cookie_rejects_malformed_header() {
        assert_eq!(session_cookie_from_set_cookie("garbage"), None);
    }

    /// A tiny Authelia-ish test server: `/api/machines` and
    /// `/api/company-hosts` 401 with a `Location` to `/login` until the
    /// right credentials are POSTed to `/api/firstfactor` (the real
    /// Authelia login endpoint `login()` posts to), at which point they
    /// require the resulting cookie.
    async fn spawn_test_server(expected_user: &'static str, expected_pass: &'static str) -> String {
        let login_count = Arc::new(AtomicU32::new(0));
        let lc = login_count.clone();

        let app = Router::new()
            .route(
                "/api/machines",
                get(move |headers: axum::http::HeaderMap| {
                    authed_response(
                        headers,
                        r#"{"machines":[{"label":"qa-1","state":"running","public_ip":"1.2.3.4","hosts_file":"1.2.3.4 example.test\n","tags":["qa"]}]}"#,
                    )
                }),
            )
            .route(
                "/api/company-hosts",
                get(|headers: axum::http::HeaderMap| authed_response(headers, "1.1.1.1 company.test\n")),
            )
            .route(
                "/api/firstfactor",
                post(move |axum::Json(payload): axum::Json<serde_json::Value>| {
                    let lc = lc.clone();
                    async move {
                        lc.fetch_add(1, Ordering::SeqCst);
                        let user = payload.get("username").and_then(|v| v.as_str());
                        let pass = payload.get("password").and_then(|v| v.as_str());
                        if user == Some(expected_user) && pass == Some(expected_pass) {
                            (
                                axum::http::StatusCode::OK,
                                [(axum::http::header::SET_COOKIE, "testsession=ok; Path=/; HttpOnly")],
                                "{}",
                            )
                                .into_response()
                        } else {
                            axum::http::StatusCode::UNAUTHORIZED.into_response()
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn authed_response(headers: axum::http::HeaderMap, body: &'static str) -> axum::response::Response {
        use axum::response::IntoResponse;
        let ok = headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            == Some("testsession=ok");
        if ok {
            body.into_response()
        } else {
            let base = headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|h| format!("http://{h}"))
                .unwrap_or_default();
            (
                axum::http::StatusCode::FOUND,
                [(axum::http::header::LOCATION, format!("{base}/auth-portal"))],
                "",
            )
                .into_response()
        }
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn fetch_machines_logs_in_automatically_on_first_401() {
        let base = spawn_test_server("alex", "hunter2").await;
        let client = test_client();

        let machines = fetch_machines(&client, &base, "alex", "hunter2").await.unwrap();

        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].label, "qa-1");
        assert_eq!(machines[0].tags, vec!["qa".to_string()]);
    }

    #[tokio::test]
    async fn fetch_machines_fails_clearly_on_wrong_credentials() {
        let base = spawn_test_server("alex", "hunter2").await;
        let client = test_client();

        let err = fetch_machines(&client, &base, "alex", "wrong").await.unwrap_err();

        assert!(matches!(err, HostsApiError::LoginFailed(_)));
    }

    #[tokio::test]
    async fn fetch_company_block_reuses_the_same_login_dance() {
        let base = spawn_test_server("alex", "hunter2").await;
        let client = test_client();

        let block = fetch_company_block(&client, &base, "alex", "hunter2").await.unwrap();

        assert_eq!(block, "1.1.1.1 company.test\n");
    }

    #[tokio::test]
    async fn fetch_machines_rejects_non_json_body_with_a_clear_error() {
        // Mirrors the real bug once hit on the other prototype: a
        // redirect silently followed would hand back an HTML login page
        // instead of JSON. Here the "logged in" response itself is
        // deliberately not JSON, exercising the parse-error path.
        let app = Router::new().route(
            "/api/machines",
            get(|| async { "<!DOCTYPE html><html>not json</html>" }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = test_client();

        let err = fetch_machines(&client, &format!("http://{addr}"), "alex", "hunter2")
            .await
            .unwrap_err();

        assert!(matches!(err, HostsApiError::BadResponse(_)));
    }

    #[test]
    fn normalize_base_url_rejects_empty_and_invalid() {
        assert!(matches!(normalize_base_url(""), Err(HostsApiError::InvalidUrl)));
        assert!(matches!(normalize_base_url("   "), Err(HostsApiError::InvalidUrl)));
        assert!(matches!(normalize_base_url("not a url"), Err(HostsApiError::InvalidUrl)));
    }

    #[test]
    fn normalize_base_url_trims_trailing_slash() {
        assert_eq!(
            normalize_base_url("https://hosts.example.test/").unwrap(),
            "https://hosts.example.test"
        );
    }
}
