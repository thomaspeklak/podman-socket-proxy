#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::Request,
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::any,
};
use http_body_util::{BodyExt, Full};
use hyper::Request as HyperRequest;
use hyper_util::{client::legacy::Client as TestClient, rt::TokioExecutor};
use hyperlocal::UnixConnector;
use podman_socket_proxy::{
    AppState, BackendConfig,
    normalize_versioned_path,
    policy::{BindMountPolicy, ImagePolicy, POLICY_SCHEMA_VERSION, Policy},
    router,
    session::{LABEL_MANAGED, LABEL_SESSION, SESSION_HEADER},
};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, UnixListener},
    sync::oneshot,
    task::JoinHandle,
};
use url::Url;

#[derive(Clone, Default)]
pub struct Calls(Arc<Mutex<Vec<(String, String)>>>);

impl Calls {
    pub fn push(&self, method: &Method, uri: &http::Uri) {
        self.0.lock().unwrap().push((
            method.to_string(),
            uri.path_and_query()
                .map(|pq| pq.as_str())
                .unwrap_or(uri.path())
                .to_string(),
        ));
    }

    pub fn snapshot(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

pub fn test_policy() -> Policy {
    let mut p = Policy {
        version: POLICY_SCHEMA_VERSION.to_string(),
        bind_mounts: BindMountPolicy {
            allowlist: vec!["/workspace".into()],
            ..Default::default()
        },
        images: ImagePolicy::default(),
        containers: Default::default(),
    };
    p.precompute();
    p
}

pub struct MockBackend {
    pub url: Url,
    pub shutdown: oneshot::Sender<()>,
    pub handle: JoinHandle<()>,
}

/// Configurable mock backend that replaces all the specialized spawners.
pub async fn spawn_mock_backend(handler: impl MockHandler + 'static) -> MockBackend {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);
    let app = Router::new().route(
        "/{*path}",
        any(move |req: Request| {
            let handler = handler.clone();
            async move { handler.handle(req).await }
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
    MockBackend {
        url: Url::parse(&format!("http://{addr}/")).unwrap(),
        shutdown: tx,
        handle,
    }
}

pub trait MockHandler: Send + Sync {
    fn handle(
        &self,
        req: Request,
    ) -> impl std::future::Future<Output = axum::response::Response> + Send;
}

/// Full lifecycle mock: create, start, inspect, logs, wait, delete
pub struct LifecycleMock {
    pub calls: Calls,
    pub sequence: AtomicUsize,
    pub inspect_body: Option<Value>,
}

impl LifecycleMock {
    pub fn new(calls: Calls) -> Self {
        Self {
            calls,
            sequence: AtomicUsize::new(1),
            inspect_body: None,
        }
    }

    pub fn with_inspect_body(calls: Calls, inspect_body: Value) -> Self {
        Self {
            calls,
            sequence: AtomicUsize::new(1),
            inspect_body: Some(inspect_body),
        }
    }
}

impl MockHandler for LifecycleMock {
    async fn handle(&self, req: Request) -> axum::response::Response {
        self.calls.push(req.method(), req.uri());
        let normalized = normalize_versioned_path(req.uri().path());
        match (req.method().clone(), normalized.as_str()) {
            (Method::POST, "/containers/create") => {
                let body = req.into_body().collect().await.unwrap().to_bytes();
                let json: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(json["Labels"][LABEL_MANAGED], "true");
                assert!(json["Labels"][LABEL_SESSION].as_str().is_some());
                let id = format!("cid-{}", self.sequence.fetch_add(1, Ordering::SeqCst));
                (StatusCode::CREATED, Json(json!({"Id": id, "Warnings": []}))).into_response()
            }
            (Method::POST, path)
                if path.starts_with("/containers/") && path.ends_with("/start") =>
            {
                StatusCode::NO_CONTENT.into_response()
            }
            (Method::GET, path)
                if path.starts_with("/containers/") && path.ends_with("/json") =>
            {
                if let Some(ref body) = self.inspect_body {
                    Json(body.clone()).into_response()
                } else {
                    let id = path
                        .trim_start_matches("/containers/")
                        .trim_end_matches("/json")
                        .trim_end_matches('/');
                    Json(json!({
                        "Id": id,
                        "State": {"Running": true},
                        "NetworkSettings": {
                            "Ports": {
                                "5432/tcp": [{"HostIp": "0.0.0.0", "HostPort": "15432"}]
                            }
                        }
                    }))
                    .into_response()
                }
            }
            (Method::GET, path)
                if path.starts_with("/containers/") && path.ends_with("/logs") =>
            {
                (StatusCode::OK, "ready\n").into_response()
            }
            (Method::POST, path)
                if path.starts_with("/containers/") && path.ends_with("/wait") =>
            {
                Json(json!({"StatusCode": 0})).into_response()
            }
            (Method::DELETE, path) if path.starts_with("/containers/") => {
                StatusCode::NO_CONTENT.into_response()
            }
            (Method::GET, "/containers/json") => {
                Json(json!([])).into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

/// Mock for startup sweep: returns stale containers for cleanup
pub struct SweepMock {
    pub calls: Calls,
}

impl MockHandler for SweepMock {
    async fn handle(&self, req: Request) -> axum::response::Response {
        self.calls.push(req.method(), req.uri());
        let normalized = normalize_versioned_path(req.uri().path());
        match (req.method().clone(), normalized.as_str()) {
            (Method::GET, "/containers/json") => {
                Json(json!([{"Id": "stale-1", "Labels": {LABEL_MANAGED: "true"}}])).into_response()
            }
            (Method::DELETE, path) if path.starts_with("/containers/") => {
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

/// Mock that returns a configurable container list for GET /containers/json
pub struct ContainerListMock {
    pub list_body: Value,
}

impl MockHandler for ContainerListMock {
    async fn handle(&self, req: Request) -> axum::response::Response {
        let normalized = normalize_versioned_path(req.uri().path());
        match (req.method().clone(), normalized.as_str()) {
            (Method::GET, "/containers/json") => Json(self.list_body.clone()).into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

/// Mock that captures the create request body
pub struct CaptureMock {
    pub captured: Arc<Mutex<Option<Value>>>,
}

impl MockHandler for CaptureMock {
    async fn handle(&self, req: Request) -> axum::response::Response {
        let normalized = normalize_versioned_path(req.uri().path());
        match (req.method().clone(), normalized.as_str()) {
            (Method::POST, "/containers/create") => {
                let body = req.into_body().collect().await.unwrap().to_bytes();
                *self.captured.lock().unwrap() =
                    Some(serde_json::from_slice::<Value>(&body).unwrap());
                (StatusCode::CREATED, Json(json!({"Id": "cid-123"}))).into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

pub async fn spawn_psp(
    socket: std::path::PathBuf,
    backend_url: Url,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    spawn_psp_with_opts(socket, backend_url, false, false).await
}

pub async fn spawn_psp_with_opts(
    socket: std::path::PathBuf,
    backend_url: Url,
    keep_on_failure: bool,
    require_session_id: bool,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
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
            keep_on_failure,
            require_session_id,
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

pub fn new_test_client() -> TestClient<UnixConnector, Full<Bytes>> {
    TestClient::builder(TokioExecutor::new()).build(UnixConnector)
}

pub async fn request_json(
    client: &TestClient<UnixConnector, Full<Bytes>>,
    socket: &std::path::Path,
    method: Method,
    path: &str,
    body: Option<Value>,
    session: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = HyperRequest::builder()
        .method(method)
        .uri::<hyper::Uri>(hyperlocal::Uri::new(socket, path).into())
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        builder = builder.header(SESSION_HEADER, session);
    }
    let request = builder
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

pub async fn request_text(
    client: &TestClient<UnixConnector, Full<Bytes>>,
    socket: &std::path::Path,
    method: Method,
    path: &str,
    session: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = HyperRequest::builder()
        .method(method)
        .uri::<hyper::Uri>(hyperlocal::Uri::new(socket, path).into());
    if let Some(session) = session {
        builder = builder.header(SESSION_HEADER, session);
    }
    let request = builder.body(Full::new(Bytes::new())).unwrap();
    let response = client.request(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

pub async fn raw_request(
    client: &TestClient<UnixConnector, Full<Bytes>>,
    socket: &std::path::Path,
    method: Method,
    path: &str,
    body: Option<Value>,
    session: Option<&str>,
) -> hyper::Response<hyper::body::Incoming> {
    let mut builder = HyperRequest::builder()
        .method(method)
        .uri::<hyper::Uri>(hyperlocal::Uri::new(socket, path).into())
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        builder = builder.header(SESSION_HEADER, session);
    }
    let request = builder
        .body(Full::new(Bytes::from(
            body.map(|v| serde_json::to_vec(&v).unwrap())
                .unwrap_or_default(),
        )))
        .unwrap();
    client.request(request).await.unwrap()
}
