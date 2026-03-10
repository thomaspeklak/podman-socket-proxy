pub mod policy;
pub mod session;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use http_body_util::{BodyExt, Full};
use hyper::Request as HyperRequest;
use hyper_util::{client::legacy::Client as HyperClient, rt::TokioExecutor};
use hyperlocal::UnixConnector;
use reqwest::Client as ReqwestClient;
use serde::Serialize;
use tokio::net::UnixListener;
use tracing::{error, info, warn};
use url::Url;

use crate::{
    policy::{Denial, Policy},
    session::{SessionManager, inject_session_labels},
};

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_socket: PathBuf,
    pub backend: BackendConfig,
    pub policy_path: PathBuf,
    pub advertised_host: String,
    pub keep_on_failure: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen_socket = std::env::var("PSP_LISTEN_SOCKET")
            .unwrap_or_else(|_| "/tmp/psp.sock".to_string())
            .into();

        let backend_raw = std::env::var("PSP_BACKEND").unwrap_or_else(|_| {
            let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| format!("/run/user/{}", std::process::id()));
            format!("unix://{runtime_dir}/podman/podman.sock")
        });

        let policy_path = std::env::var("PSP_POLICY_FILE")
            .unwrap_or_else(|_| "policy/default-policy.json".to_string())
            .into();

        let advertised_host =
            std::env::var("PSP_ADVERTISED_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

        let keep_on_failure = std::env::var("PSP_KEEP_ON_FAILURE")
            .ok()
            .as_deref()
            .map(|value| matches!(value, "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);

        Ok(Self {
            listen_socket,
            backend: BackendConfig::parse(&backend_raw)?,
            policy_path,
            advertised_host,
            keep_on_failure,
        })
    }
}

#[derive(Clone, Debug)]
pub enum BackendConfig {
    Http(Url),
    Unix(PathBuf),
}

impl BackendConfig {
    pub fn parse(value: &str) -> Result<Self> {
        if let Some(path) = value.strip_prefix("unix://") {
            return Ok(Self::Unix(PathBuf::from(path)));
        }

        let url = Url::parse(value).with_context(|| format!("invalid backend url: {value}"))?;
        match url.scheme() {
            "http" | "https" => Ok(Self::Http(url)),
            other => Err(anyhow!("unsupported backend scheme: {other}")),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    backend: BackendClient,
    policy: Policy,
    advertised_host: String,
    sessions: SessionManager,
}

impl AppState {
    pub fn new(
        backend: BackendConfig,
        policy: Policy,
        advertised_host: impl Into<String>,
        keep_on_failure: bool,
    ) -> Result<Self> {
        Ok(Self {
            backend: BackendClient::new(backend)?,
            policy,
            advertised_host: advertised_host.into(),
            sessions: SessionManager::new(keep_on_failure),
        })
    }

    async fn startup_sweep(&self) -> Result<(), ProxyError> {
        let response = self
            .backend
            .get_json("/containers/json?all=1&filters=%7B%22label%22%3A%5B%22io.psp.managed%3Dtrue%22%5D%7D")
            .await?;

        let Some(containers) = response.as_array() else {
            return Ok(());
        };

        for container in containers {
            if let Some(id) = container.get("Id").and_then(|value| value.as_str()) {
                self.backend
                    .delete(&format!("/containers/{id}?force=1"))
                    .await?;
            }
        }

        Ok(())
    }

    async fn cleanup_tracked_resources(&self) -> Result<(), ProxyError> {
        if self.sessions.keep_on_failure() {
            return Ok(());
        }

        for id in self.sessions.tracked_container_ids() {
            self.backend
                .delete(&format!("/containers/{id}?force=1"))
                .await?;
        }

        Ok(())
    }
}

#[derive(Clone)]
enum BackendClient {
    Http {
        client: ReqwestClient,
        base: Url,
    },
    Unix {
        client: HyperClient<UnixConnector, Full<Bytes>>,
        socket: PathBuf,
    },
}

impl BackendClient {
    fn new(config: BackendConfig) -> Result<Self> {
        Ok(match config {
            BackendConfig::Http(base) => Self::Http {
                client: ReqwestClient::builder().build()?,
                base,
            },
            BackendConfig::Unix(socket) => Self::Unix {
                client: HyperClient::builder(TokioExecutor::new()).build(UnixConnector),
                socket,
            },
        })
    }

    async fn send(
        &self,
        method: Method,
        uri: &Uri,
        headers: &http::HeaderMap,
        body: Bytes,
    ) -> Result<ForwardedResponse, ProxyError> {
        match self {
            Self::Http { client, base } => {
                let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
                let url = base
                    .join(path_and_query.trim_start_matches('/'))
                    .map_err(ProxyError::internal)?;
                let mut upstream = client.request(method, url);
                for (name, value) in headers {
                    if name.as_str().eq_ignore_ascii_case("host")
                        || name == http::header::CONTENT_LENGTH
                    {
                        continue;
                    }
                    upstream = upstream.header(name, value);
                }
                let response = upstream
                    .body(body)
                    .send()
                    .await
                    .map_err(ProxyError::backend)?;
                Ok(ForwardedResponse {
                    status: response.status(),
                    headers: response.headers().clone(),
                    body: response.bytes().await.map_err(ProxyError::backend)?,
                })
            }
            Self::Unix { client, socket } => {
                let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
                let upstream_uri: hyper::Uri = hyperlocal::Uri::new(socket, path_and_query).into();
                let mut request_builder = HyperRequest::builder().method(method).uri(upstream_uri);
                for (name, value) in headers {
                    if name.as_str().eq_ignore_ascii_case("host")
                        || name == http::header::CONTENT_LENGTH
                    {
                        continue;
                    }
                    request_builder = request_builder.header(name, value);
                }
                let request = request_builder
                    .body(Full::new(body))
                    .map_err(ProxyError::internal)?;
                let response = client
                    .request(request)
                    .await
                    .map_err(ProxyError::hyper_backend)?;
                let (parts, body) = response.into_parts();
                let body = body
                    .collect()
                    .await
                    .map_err(ProxyError::hyper_backend)?
                    .to_bytes();
                Ok(ForwardedResponse {
                    status: parts.status,
                    headers: parts.headers,
                    body,
                })
            }
        }
    }

    async fn get_json(&self, path_and_query: &str) -> Result<serde_json::Value, ProxyError> {
        let uri: Uri = path_and_query.parse().map_err(ProxyError::internal)?;
        let response = self
            .send(Method::GET, &uri, &http::HeaderMap::new(), Bytes::new())
            .await?;
        serde_json::from_slice(&response.body).map_err(ProxyError::internal)
    }

    async fn delete(&self, path_and_query: &str) -> Result<(), ProxyError> {
        let uri: Uri = path_and_query.parse().map_err(ProxyError::internal)?;
        self.send(Method::DELETE, &uri, &http::HeaderMap::new(), Bytes::new())
            .await?;
        Ok(())
    }
}

struct ForwardedResponse {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
}

#[derive(Clone, Debug, Default)]
struct RequestAuditContext {
    session_id: String,
    operation: String,
    path: String,
    target_image: Option<String>,
    target_container: Option<String>,
}

impl RequestAuditContext {
    fn from_request(
        method: &Method,
        path: &str,
        query: Option<&str>,
        body: &[u8],
        session_id: String,
    ) -> Self {
        Self {
            session_id,
            operation: operation_name(method, path),
            path: path.to_string(),
            target_image: extract_target_image(method, path, query, body),
            target_container: extract_target_container(path),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", any(proxy_request))
        .route("/{*path}", any(proxy_request))
        .with_state(Arc::new(state))
}

async fn proxy_request(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response<Body>, ProxyError> {
    if !is_supported_endpoint(request.method(), request.uri().path()) {
        let session_id = state.sessions.session_id(request.headers());
        let audit = RequestAuditContext::from_request(
            request.method(),
            request.uri().path(),
            request.uri().query(),
            &[],
            session_id,
        );
        warn!(
            decision = "deny",
            kind = "unsupported_endpoint",
            session = %audit.session_id,
            operation = %audit.operation,
            path = %audit.path,
            target_image = audit.target_image.as_deref().unwrap_or(""),
            target_container = audit.target_container.as_deref().unwrap_or(""),
            "psp denied request"
        );
        return Err(ProxyError::unsupported(
            request.method().clone(),
            request.uri().path(),
        ));
    }

    let (parts, body) = request.into_parts();
    let session_id = state.sessions.session_id(&parts.headers);
    let body = body
        .collect()
        .await
        .map_err(ProxyError::internal)?
        .to_bytes();
    let audit = RequestAuditContext::from_request(
        &parts.method,
        parts.uri.path(),
        parts.uri.query(),
        body.as_ref(),
        session_id.clone(),
    );

    if let Err(denial) = state.policy.evaluate_request(
        &parts.method,
        parts.uri.path(),
        parts.uri.query(),
        body.as_ref(),
    ) {
        warn!(
            decision = "deny",
            kind = "policy_denied",
            rule_id = denial.rule_id,
            session = %audit.session_id,
            operation = %audit.operation,
            path = %audit.path,
            target_image = audit.target_image.as_deref().unwrap_or(""),
            target_container = audit.target_container.as_deref().unwrap_or(""),
            reason = %denial.reason,
            "psp denied request"
        );
        return Err(ProxyError::policy_denied(denial));
    }

    let body = maybe_inject_session_labels(&parts.method, parts.uri.path(), body, &session_id)
        .map_err(ProxyError::internal)?;

    let method = parts.method.clone();
    let normalized_path = normalize_versioned_path(parts.uri.path());
    let upstream = state
        .backend
        .send(parts.method, &parts.uri, &parts.headers, body)
        .await?;

    if method == Method::POST
        && normalized_path == "/containers/create"
        && upstream.status == StatusCode::CREATED
    {
        if let Some(id) = extract_container_id(&upstream.body) {
            state.sessions.track_container(&session_id, &id);
        }
    }

    if method == Method::DELETE
        && normalized_path.starts_with("/containers/")
        && upstream.status.is_success()
    {
        if let Some(id) = container_id_from_path(&normalized_path) {
            state.sessions.untrack_container(id);
        }
    }

    info!(
        decision = "allow",
        session = %audit.session_id,
        operation = %audit.operation,
        path = %audit.path,
        target_image = audit.target_image.as_deref().unwrap_or(""),
        target_container = audit.target_container.as_deref().unwrap_or(""),
        status = upstream.status.as_u16(),
        "psp forwarded request"
    );

    let body = rewrite_response_body(
        &method,
        parts.uri.path(),
        upstream.status,
        &state.advertised_host,
        upstream.body,
    );

    let mut response = Response::builder().status(upstream.status);
    for (name, value) in &upstream.headers {
        if hop_by_hop_header(name) || name == http::header::CONTENT_LENGTH {
            continue;
        }
        response = response.header(name, value);
    }
    response = response.header(http::header::CONTENT_LENGTH, body.len());

    response
        .body(Body::from(body))
        .map_err(ProxyError::internal)
}

pub async fn serve_with_shutdown(config: Config) -> Result<()> {
    if let Some(parent) = config.listen_socket.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create socket dir {}", parent.display()))?;
    }

    remove_existing_socket(&config.listen_socket).await?;

    let listener = UnixListener::bind(&config.listen_socket)
        .with_context(|| format!("failed to bind socket {}", config.listen_socket.display()))?;

    let policy = Policy::load(&config.policy_path).with_context(|| {
        format!(
            "failed to load policy from {}",
            config.policy_path.display()
        )
    })?;

    info!(socket = %config.listen_socket.display(), policy = %config.policy_path.display(), advertised_host = %config.advertised_host, keep_on_failure = config.keep_on_failure, "psp listening");

    let state = AppState::new(
        config.backend,
        policy,
        config.advertised_host,
        config.keep_on_failure,
    )?;
    state
        .startup_sweep()
        .await
        .map_err(|error| anyhow!("startup sweep failed: {error:?}"))?;

    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("psp server exited unexpectedly")?;

    state
        .cleanup_tracked_resources()
        .await
        .map_err(|error| anyhow!("shutdown cleanup failed: {error:?}"))?;

    Ok(())
}

async fn remove_existing_socket(path: &Path) -> Result<()> {
    if path.exists() {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
    }
    Ok(())
}

pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        sigterm.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

pub fn is_supported_endpoint(method: &Method, path: &str) -> bool {
    let normalized = normalize_versioned_path(path);
    match (method.as_str(), normalized.as_str()) {
        ("GET", "/_ping") | ("GET", "/version") | ("GET", "/info") | ("POST", "/images/create") => {
            true
        }
        _ if method == Method::POST && normalized == "/containers/create" => true,
        _ if method == Method::GET
            && normalized.starts_with("/containers/")
            && normalized.ends_with("/json") =>
        {
            true
        }
        _ if method == Method::POST
            && normalized.starts_with("/containers/")
            && normalized.ends_with("/start") =>
        {
            true
        }
        _ if method == Method::GET
            && normalized.starts_with("/containers/")
            && normalized.ends_with("/logs") =>
        {
            true
        }
        _ if method == Method::POST
            && normalized.starts_with("/containers/")
            && normalized.ends_with("/wait") =>
        {
            true
        }
        _ if method == Method::DELETE
            && normalized.starts_with("/containers/")
            && path_segment_count(&normalized) == 2 =>
        {
            true
        }
        _ => false,
    }
}

pub fn normalize_versioned_path(path: &str) -> String {
    let trimmed = if path.is_empty() { "/" } else { path };
    let segments: Vec<&str> = trimmed.trim_start_matches('/').split('/').collect();
    if let Some(first) = segments.first()
        && first.starts_with('v')
        && first[1..]
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch == '.')
        && segments.len() > 1
    {
        return format!("/{}", segments[1..].join("/"));
    }
    trimmed.to_string()
}

fn path_segment_count(path: &str) -> usize {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .count()
}

fn maybe_inject_session_labels(
    method: &Method,
    path: &str,
    body: Bytes,
    session_id: &str,
) -> Result<Bytes> {
    if method == Method::POST && normalize_versioned_path(path) == "/containers/create" {
        return inject_session_labels(body, session_id);
    }
    Ok(body)
}

fn extract_container_id(body: &Bytes) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|json| {
            json.get("Id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn operation_name(method: &Method, path: &str) -> String {
    let normalized = normalize_versioned_path(path);
    match (method.as_str(), normalized.as_str()) {
        ("GET", "/_ping") => "daemon.ping".into(),
        ("GET", "/version") => "daemon.version".into(),
        ("GET", "/info") => "daemon.info".into(),
        ("POST", "/images/create") => "images.create".into(),
        ("POST", "/containers/create") => "containers.create".into(),
        _ if method == Method::POST && normalized.ends_with("/start") => "containers.start".into(),
        _ if method == Method::GET && normalized.ends_with("/json") => "containers.inspect".into(),
        _ if method == Method::GET && normalized.ends_with("/logs") => "containers.logs".into(),
        _ if method == Method::POST && normalized.ends_with("/wait") => "containers.wait".into(),
        _ if method == Method::DELETE && normalized.starts_with("/containers/") => {
            "containers.delete".into()
        }
        _ => format!("{} {}", method, normalized),
    }
}

fn extract_target_image(
    method: &Method,
    path: &str,
    query: Option<&str>,
    body: &[u8],
) -> Option<String> {
    let normalized = normalize_versioned_path(path);
    if method == Method::POST && normalized == "/images/create" {
        return query.and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "fromImage")
                .map(|(_, value)| value.into_owned())
        });
    }

    if method == Method::POST && normalized == "/containers/create" {
        return serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|json| {
                json.get("Image")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            });
    }

    None
}

fn extract_target_container(path: &str) -> Option<String> {
    let normalized = normalize_versioned_path(path);
    let trimmed = normalized.strip_prefix("/containers/")?;
    let id = trimmed.split('/').next()?;
    if id == "create" {
        None
    } else {
        Some(id.to_string())
    }
}

fn container_id_from_path(path: &str) -> Option<&str> {
    let trimmed = path.trim_start_matches("/containers/");
    if trimmed.contains('/') {
        None
    } else {
        Some(trimmed)
    }
}

fn rewrite_response_body(
    method: &Method,
    path: &str,
    status: StatusCode,
    advertised_host: &str,
    body: Bytes,
) -> Bytes {
    if method != Method::GET || status != StatusCode::OK {
        return body;
    }

    let normalized = normalize_versioned_path(path);
    if !(normalized.starts_with("/containers/") && normalized.ends_with("/json")) {
        return body;
    }

    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };

    let Some(ports) = json
        .get_mut("NetworkSettings")
        .and_then(|network_settings| network_settings.get_mut("Ports"))
        .and_then(|ports| ports.as_object_mut())
    else {
        return body;
    };

    for entries in ports.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        for entry in entries {
            if let Some(host_ip) = entry.get_mut("HostIp") {
                *host_ip = serde_json::Value::String(advertised_host.to_string());
            }
        }
    }

    serde_json::to_vec(&json).map(Bytes::from).unwrap_or(body)
}

#[derive(Debug)]
pub enum ProxyError {
    Unsupported { method: Method, path: String },
    PolicyDenied(Denial),
    Backend(String),
    Internal(anyhow::Error),
}

impl ProxyError {
    fn unsupported(method: Method, path: &str) -> Self {
        Self::Unsupported {
            method,
            path: path.to_string(),
        }
    }

    fn policy_denied(denial: Denial) -> Self {
        Self::PolicyDenied(denial)
    }

    fn backend(error: reqwest::Error) -> Self {
        Self::Backend(error.to_string())
    }

    fn hyper_backend<E: std::fmt::Display>(error: E) -> Self {
        Self::Backend(error.to_string())
    }

    fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: String,
    kind: &'a str,
    method: Option<String>,
    path: Option<String>,
    rule_id: Option<&'a str>,
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response<Body> {
        match self {
            Self::Unsupported { method, path } => json_response(
                StatusCode::NOT_IMPLEMENTED,
                &ErrorBody {
                    message: format!("unsupported endpoint: {} {}", method, path),
                    kind: "unsupported_endpoint",
                    method: Some(method.to_string()),
                    path: Some(path),
                    rule_id: None,
                },
            ),
            Self::PolicyDenied(denial) => json_response(
                StatusCode::FORBIDDEN,
                &ErrorBody {
                    message: denial.reason,
                    kind: "policy_denied",
                    method: None,
                    path: None,
                    rule_id: Some(denial.rule_id),
                },
            ),
            Self::Backend(message) => json_response(
                StatusCode::BAD_GATEWAY,
                &ErrorBody {
                    message: format!("backend request failed: {message}"),
                    kind: "backend_error",
                    method: None,
                    path: None,
                    rule_id: None,
                },
            ),
            Self::Internal(error) => {
                error!(error = ?error, "internal proxy error");
                json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorBody {
                        message: "internal proxy error".to_string(),
                        kind: "internal_error",
                        method: None,
                        path: None,
                        rule_id: None,
                    },
                )
            }
        }
    }
}

fn json_response(status: StatusCode, body: &impl Serialize) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(
            serde_json::to_vec(body).expect("serialize error body"),
        ))
        .expect("build json response")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use axum::{Json, routing::any};
    use hyper_util::{client::legacy::Client as TestClient, rt::TokioExecutor};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

    #[derive(Clone, Default)]
    struct Calls(Arc<Mutex<Vec<(String, String)>>>);

    impl Calls {
        fn push(&self, method: &Method, uri: &Uri) {
            self.0.lock().unwrap().push((
                method.to_string(),
                uri.path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or(uri.path())
                    .to_string(),
            ));
        }

        fn snapshot(&self) -> Vec<(String, String)> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn proxies_container_lifecycle_endpoints() {
        let calls = Calls::default();
        let (backend_url, backend_shutdown, backend_handle) = spawn_backend(calls.clone()).await;
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("psp.sock");
        let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend_url).await;

        let client: TestClient<_, Full<Bytes>> =
            TestClient::builder(TokioExecutor::new()).build(UnixConnector);

        let create = request_json(
            &client,
            &socket,
            Method::POST,
            "/v1.41/containers/create?name=test",
            Some(json!({"Image":"postgres:16"})),
        )
        .await;
        assert_eq!(create.0, StatusCode::CREATED);
        assert_eq!(create.1["Id"], "cid-123");

        let start = request_text(
            &client,
            &socket,
            Method::POST,
            "/v1.41/containers/cid-123/start",
        )
        .await;
        assert_eq!(start.0, StatusCode::NO_CONTENT);

        let inspect = request_json(
            &client,
            &socket,
            Method::GET,
            "/v1.41/containers/cid-123/json",
            None,
        )
        .await;
        assert_eq!(inspect.0, StatusCode::OK);
        assert_eq!(inspect.1["Id"], "cid-123");

        let logs = request_text(
            &client,
            &socket,
            Method::GET,
            "/v1.41/containers/cid-123/logs?stdout=1&stderr=1",
        )
        .await;
        assert_eq!(logs.0, StatusCode::OK);
        assert_eq!(logs.1, "ready\n");

        let wait = request_json(
            &client,
            &socket,
            Method::POST,
            "/v1.41/containers/cid-123/wait",
            None,
        )
        .await;
        assert_eq!(wait.0, StatusCode::OK);
        assert_eq!(wait.1["StatusCode"], 0);

        let remove = request_text(
            &client,
            &socket,
            Method::DELETE,
            "/v1.41/containers/cid-123?force=1",
        )
        .await;
        assert_eq!(remove.0, StatusCode::NO_CONTENT);
        assert_eq!(remove.1, "");

        assert_eq!(
            calls.snapshot(),
            vec![
                ("POST".into(), "/v1.41/containers/create?name=test".into()),
                ("POST".into(), "/v1.41/containers/cid-123/start".into()),
                ("GET".into(), "/v1.41/containers/cid-123/json".into()),
                (
                    "GET".into(),
                    "/v1.41/containers/cid-123/logs?stdout=1&stderr=1".into()
                ),
                ("POST".into(), "/v1.41/containers/cid-123/wait".into()),
                ("DELETE".into(), "/v1.41/containers/cid-123?force=1".into()),
            ]
        );

        let _ = psp_shutdown.send(());
        let _ = backend_shutdown.send(());
        let _ = psp_handle.await;
        let _ = backend_handle.await;
    }

    #[tokio::test]
    async fn rewrites_container_inspect_host_ports_for_client_connectivity() {
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        let port_b = listener_b.local_addr().unwrap().port();
        let _accept_a = tokio::spawn(async move {
            let _ = listener_a.accept().await;
        });
        let _accept_b = tokio::spawn(async move {
            let _ = listener_b.accept().await;
        });

        let inspect_body = json!({
            "Id": "cid-123",
            "NetworkSettings": {
                "Ports": {
                    "5432/tcp": [{"HostIp": "0.0.0.0", "HostPort": port_a.to_string()}],
                    "8080/tcp": [{"HostIp": "0.0.0.0", "HostPort": port_b.to_string()}]
                }
            }
        });
        let (backend_url, backend_shutdown, backend_handle) =
            spawn_inspect_backend(inspect_body).await;

        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("psp.sock");
        let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend_url).await;
        let client: TestClient<_, Full<Bytes>> =
            TestClient::builder(TokioExecutor::new()).build(UnixConnector);

        let inspect = request_json(
            &client,
            &socket,
            Method::GET,
            "/v1.41/containers/cid-123/json",
            None,
        )
        .await;
        assert_eq!(inspect.0, StatusCode::OK);
        assert_eq!(
            inspect.1["NetworkSettings"]["Ports"]["5432/tcp"][0]["HostIp"],
            "127.0.0.1"
        );
        assert_eq!(
            inspect.1["NetworkSettings"]["Ports"]["8080/tcp"][0]["HostIp"],
            "127.0.0.1"
        );

        let endpoints = [port_a, port_b].map(|port| format!("127.0.0.1:{port}"));
        let (a, b) = tokio::join!(
            tokio::net::TcpStream::connect(&endpoints[0]),
            tokio::net::TcpStream::connect(&endpoints[1]),
        );
        assert!(a.is_ok());
        assert!(b.is_ok());

        let _ = psp_shutdown.send(());
        let _ = backend_shutdown.send(());
        let _ = psp_handle.await;
        let _ = backend_handle.await;
    }

    #[tokio::test]
    async fn injects_session_labels_on_container_create() {
        let captured = Arc::new(Mutex::new(None::<Value>));
        let (backend_url, backend_shutdown, backend_handle) =
            spawn_create_backend(captured.clone()).await;
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("psp.sock");
        let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend_url).await;

        let client: TestClient<_, Full<Bytes>> =
            TestClient::builder(TokioExecutor::new()).build(UnixConnector);
        let request = HyperRequest::builder()
            .method(Method::POST)
            .uri::<hyper::Uri>(hyperlocal::Uri::new(&socket, "/v1.41/containers/create").into())
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(session::SESSION_HEADER, "sess-123")
            .body(Full::new(Bytes::from(
                serde_json::to_vec(&json!({"Image":"postgres:16"})).unwrap(),
            )))
            .unwrap();
        let response = client.request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let captured = captured.lock().unwrap().clone().unwrap();
        assert_eq!(captured["Labels"][session::LABEL_MANAGED], "true");
        assert_eq!(captured["Labels"][session::LABEL_SESSION], "sess-123");

        let _ = psp_shutdown.send(());
        let _ = backend_shutdown.send(());
        let _ = psp_handle.await;
        let _ = backend_handle.await;
    }

    #[tokio::test]
    async fn startup_sweep_removes_stale_managed_containers() {
        let calls = Calls::default();
        let (backend_url, backend_shutdown, backend_handle) =
            spawn_sweep_backend(calls.clone()).await;
        let state = AppState::new(
            BackendConfig::Http(backend_url),
            test_policy(),
            "127.0.0.1",
            false,
        )
        .unwrap();
        state.startup_sweep().await.unwrap();

        assert_eq!(
            calls.snapshot(),
            vec![
                (
                    "GET".into(),
                    "/containers/json?all=1&filters=%7B%22label%22%3A%5B%22io.psp.managed%3Dtrue%22%5D%7D"
                        .into(),
                ),
                ("DELETE".into(), "/containers/stale-1?force=1".into()),
            ]
        );

        let _ = backend_shutdown.send(());
        let _ = backend_handle.await;
    }

    #[tokio::test]
    async fn cleanup_tracked_resources_deletes_containers_on_shutdown() {
        let calls = Calls::default();
        let (backend_url, backend_shutdown, backend_handle) =
            spawn_delete_backend(calls.clone()).await;
        let state = AppState::new(
            BackendConfig::Http(backend_url),
            test_policy(),
            "127.0.0.1",
            false,
        )
        .unwrap();
        state.sessions.track_container("sess-1", "cid-123");
        state.cleanup_tracked_resources().await.unwrap();

        assert_eq!(
            calls.snapshot(),
            vec![("DELETE".into(), "/containers/cid-123?force=1".into())]
        );

        let _ = backend_shutdown.send(());
        let _ = backend_handle.await;
    }

    #[test]
    fn derives_secret_safe_audit_context() {
        let body =
            serde_json::to_vec(&json!({"Image":"postgres:16","Env":["TOKEN=secret"]})).unwrap();
        let audit = RequestAuditContext::from_request(
            &Method::POST,
            "/v1.41/containers/create",
            None,
            &body,
            "sess-a".into(),
        );
        assert_eq!(audit.session_id, "sess-a");
        assert_eq!(audit.operation, "containers.create");
        assert_eq!(audit.target_image.as_deref(), Some("postgres:16"));
        assert_eq!(audit.target_container, None);
    }

    #[tokio::test]
    async fn accepts_versioned_and_unversioned_supported_paths() {
        assert!(is_supported_endpoint(&Method::GET, "/_ping"));
        assert!(is_supported_endpoint(&Method::GET, "/v1.41/_ping"));
        assert!(is_supported_endpoint(&Method::POST, "/containers/create"));
        assert!(is_supported_endpoint(
            &Method::POST,
            "/v1.41/containers/create"
        ));
        assert!(is_supported_endpoint(
            &Method::DELETE,
            "/v1.41/containers/cid-123"
        ));
        assert!(!is_supported_endpoint(
            &Method::POST,
            "/v1.41/networks/create"
        ));
    }

    #[tokio::test]
    async fn denies_policy_blocked_container_create_with_rule_id() {
        let calls = Calls::default();
        let (backend_url, backend_shutdown, backend_handle) = spawn_backend(calls.clone()).await;
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("psp.sock");
        let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend_url).await;

        let client: TestClient<_, Full<Bytes>> =
            TestClient::builder(TokioExecutor::new()).build(UnixConnector);
        let response = request_json(
            &client,
            &socket,
            Method::POST,
            "/v1.41/containers/create",
            Some(json!({
                "Image": "postgres:16",
                "HostConfig": {"Privileged": true}
            })),
        )
        .await;
        assert_eq!(response.0, StatusCode::FORBIDDEN);
        assert_eq!(response.1["kind"], "policy_denied");
        assert_eq!(response.1["rule_id"], policy::RULE_PRIVILEGED);
        assert!(calls.snapshot().is_empty());

        let _ = psp_shutdown.send(());
        let _ = backend_shutdown.send(());
        let _ = psp_handle.await;
        let _ = backend_handle.await;
    }

    #[tokio::test]
    async fn rejects_unsupported_endpoints_with_structured_error() {
        let calls = Calls::default();
        let (backend_url, backend_shutdown, backend_handle) = spawn_backend(calls.clone()).await;
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("psp.sock");
        let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend_url).await;

        let client: TestClient<_, Full<Bytes>> =
            TestClient::builder(TokioExecutor::new()).build(UnixConnector);
        let response = request_json(
            &client,
            &socket,
            Method::POST,
            "/v1.41/networks/create",
            Some(json!({"Name":"n1"})),
        )
        .await;
        assert_eq!(response.0, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(response.1["kind"], "unsupported_endpoint");
        assert_eq!(response.1["path"], "/v1.41/networks/create");
        assert!(calls.snapshot().is_empty());

        let _ = psp_shutdown.send(());
        let _ = backend_shutdown.send(());
        let _ = psp_handle.await;
        let _ = backend_handle.await;
    }

    async fn spawn_psp(socket: PathBuf, backend_url: Url) -> (oneshot::Sender<()>, JoinHandle<()>) {
        if let Some(parent) = socket.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        let _ = tokio::fs::remove_file(&socket).await;
        let listener = UnixListener::bind(&socket).unwrap();
        let app = router(
            AppState::new(
                BackendConfig::Http(backend_url),
                test_policy(),
                "127.0.0.1",
                false,
            )
            .unwrap(),
        );
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (tx, handle)
    }

    fn test_policy() -> Policy {
        Policy {
            version: policy::POLICY_SCHEMA_VERSION.to_string(),
            bind_mounts: policy::BindMountPolicy {
                allowlist: vec!["/workspace".into()],
            },
            images: policy::ImagePolicy::default(),
        }
    }

    async fn spawn_create_backend(
        captured: Arc<Mutex<Option<Value>>>,
    ) -> (Url, oneshot::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/{*path}",
            any(move |req: Request| {
                let captured = captured.clone();
                async move {
                    let normalized = normalize_versioned_path(req.uri().path());
                    match (req.method().clone(), normalized.as_str()) {
                        (Method::POST, "/containers/create") => {
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            *captured.lock().unwrap() =
                                Some(serde_json::from_slice::<Value>(&body).unwrap());
                            (StatusCode::CREATED, Json(json!({"Id":"cid-123"}))).into_response()
                        }
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (Url::parse(&format!("http://{addr}/")).unwrap(), tx, handle)
    }

    async fn spawn_sweep_backend(calls: Calls) -> (Url, oneshot::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/{*path}",
            any(move |req: Request| {
                let calls = calls.clone();
                async move {
                    calls.push(req.method(), req.uri());
                    let normalized = normalize_versioned_path(req.uri().path());
                    match (req.method().clone(), normalized.as_str()) {
                        (Method::GET, "/containers/json") => Json(json!([
                            {"Id":"stale-1","Labels":{session::LABEL_MANAGED:"true"}}
                        ]))
                        .into_response(),
                        (Method::DELETE, "/containers/stale-1") => {
                            StatusCode::NO_CONTENT.into_response()
                        }
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (Url::parse(&format!("http://{addr}/")).unwrap(), tx, handle)
    }

    async fn spawn_delete_backend(calls: Calls) -> (Url, oneshot::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/{*path}",
            any(move |req: Request| {
                let calls = calls.clone();
                async move {
                    calls.push(req.method(), req.uri());
                    match req.method() {
                        &Method::DELETE => StatusCode::NO_CONTENT.into_response(),
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (Url::parse(&format!("http://{addr}/")).unwrap(), tx, handle)
    }

    async fn spawn_inspect_backend(
        inspect_body: Value,
    ) -> (Url, oneshot::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/{*path}",
            any(move |req: Request| {
                let inspect_body = inspect_body.clone();
                async move {
                    let normalized = normalize_versioned_path(req.uri().path());
                    match (req.method().clone(), normalized.as_str()) {
                        (Method::GET, path)
                            if path.starts_with("/containers/") && path.ends_with("/json") =>
                        {
                            Json(inspect_body).into_response()
                        }
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (Url::parse(&format!("http://{addr}/")).unwrap(), tx, handle)
    }

    async fn spawn_backend(calls: Calls) -> (Url, oneshot::Sender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/{*path}",
            any({
                let calls = calls.clone();
                move |req: Request| {
                    let calls = calls.clone();
                    async move {
                        calls.push(req.method(), req.uri());
                        let normalized = normalize_versioned_path(req.uri().path());
                        match (req.method().clone(), normalized.as_str()) {
                            (Method::POST, "/containers/create") => (
                                StatusCode::CREATED,
                                Json(json!({"Id":"cid-123","Warnings":[]})),
                            )
                                .into_response(),
                            (Method::GET, path)
                                if path.starts_with("/containers/") && path.ends_with("/json") =>
                            {
                                let id = path
                                    .trim_start_matches("/containers/")
                                    .trim_end_matches("/json")
                                    .trim_end_matches('/');
                                Json(json!({"Id":id,"State":{"Running":true}})).into_response()
                            }
                            (Method::POST, path)
                                if path.starts_with("/containers/") && path.ends_with("/start") =>
                            {
                                StatusCode::NO_CONTENT.into_response()
                            }
                            (Method::GET, path)
                                if path.starts_with("/containers/") && path.ends_with("/logs") =>
                            {
                                (StatusCode::OK, "ready\n").into_response()
                            }
                            (Method::POST, path)
                                if path.starts_with("/containers/") && path.ends_with("/wait") =>
                            {
                                Json(json!({"StatusCode":0})).into_response()
                            }
                            (Method::DELETE, path)
                                if path.starts_with("/containers/")
                                    && path_segment_count(path) == 2 =>
                            {
                                StatusCode::NO_CONTENT.into_response()
                            }
                            _ => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }
            }),
        );

        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });

        (Url::parse(&format!("http://{addr}/")).unwrap(), tx, handle)
    }

    async fn request_json(
        client: &TestClient<UnixConnector, Full<Bytes>>,
        socket: &Path,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = HyperRequest::builder()
            .method(method)
            .uri::<hyper::Uri>(hyperlocal::Uri::new(socket, path).into())
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                body.map(|v| serde_json::to_vec(&v).unwrap())
                    .unwrap_or_default(),
            )))
            .unwrap();
        let response = client.request(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    async fn request_text(
        client: &TestClient<UnixConnector, Full<Bytes>>,
        socket: &Path,
        method: Method,
        path: &str,
    ) -> (StatusCode, String) {
        let request = HyperRequest::builder()
            .method(method)
            .uri::<hyper::Uri>(hyperlocal::Uri::new(socket, path).into())
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = client.request(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}
