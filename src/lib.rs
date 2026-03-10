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
use tracing::{error, info};
use url::Url;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_socket: PathBuf,
    pub backend: BackendConfig,
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

        Ok(Self {
            listen_socket,
            backend: BackendConfig::parse(&backend_raw)?,
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
}

impl AppState {
    pub fn new(backend: BackendConfig) -> Result<Self> {
        Ok(Self {
            backend: BackendClient::new(backend)?,
        })
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
                    if name.as_str().eq_ignore_ascii_case("host") {
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
                    if name.as_str().eq_ignore_ascii_case("host") {
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
}

struct ForwardedResponse {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
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
        return Err(ProxyError::unsupported(
            request.method().clone(),
            request.uri().path(),
        ));
    }

    let (parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map_err(ProxyError::internal)?
        .to_bytes();

    let upstream = state
        .backend
        .send(parts.method, &parts.uri, &parts.headers, body)
        .await?;

    let mut response = Response::builder().status(upstream.status);
    for (name, value) in &upstream.headers {
        if hop_by_hop_header(name) {
            continue;
        }
        response = response.header(name, value);
    }

    response
        .body(Body::from(upstream.body))
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

    info!(socket = %config.listen_socket.display(), "psp listening");

    axum::serve(listener, router(AppState::new(config.backend)?))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("psp server exited unexpectedly")?;

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

#[derive(Debug)]
pub enum ProxyError {
    Unsupported { method: Method, path: String },
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
                },
            ),
            Self::Backend(message) => json_response(
                StatusCode::BAD_GATEWAY,
                &ErrorBody {
                    message: format!("backend request failed: {message}"),
                    kind: "backend_error",
                    method: None,
                    path: None,
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
        let app = router(AppState::new(BackendConfig::Http(backend_url)).unwrap());
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
