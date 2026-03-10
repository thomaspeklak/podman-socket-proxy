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
    policy::{BindMountPolicy, ImagePolicy, POLICY_SCHEMA_VERSION, Policy},
    router,
    session::{LABEL_MANAGED, LABEL_SESSION, SESSION_HEADER},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    net::{TcpListener, UnixListener},
    sync::oneshot,
    task::JoinHandle,
};
use url::Url;

#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<(String, String)>>>);

impl Calls {
    fn push(&self, method: &Method, uri: &http::Uri) {
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

fn test_policy() -> Policy {
    Policy {
        version: POLICY_SCHEMA_VERSION.to_string(),
        bind_mounts: BindMountPolicy {
            allowlist: vec!["/workspace".into()],
        },
        images: ImagePolicy::default(),
    }
}

#[tokio::test]
async fn compatibility_suite_covers_happy_and_blocked_paths() {
    let calls = Calls::default();
    let (backend_url, backend_shutdown, backend_handle) =
        spawn_lifecycle_backend(calls.clone()).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle, _state) = spawn_psp(socket.clone(), backend_url, false).await;
    let client: TestClient<_, Full<Bytes>> =
        TestClient::builder(TokioExecutor::new()).build(UnixConnector);

    let create = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create?name=test",
        Some(json!({"Image":"postgres:16"})),
        Some("sess-happy"),
    )
    .await;
    assert_eq!(create.0, StatusCode::CREATED);

    let start = request_text(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/cid-1/start",
        None,
    )
    .await;
    assert_eq!(start.0, StatusCode::NO_CONTENT);

    let inspect = request_json(
        &client,
        &socket,
        Method::GET,
        "/v1.41/containers/cid-1/json",
        None,
        None,
    )
    .await;
    assert_eq!(inspect.0, StatusCode::OK);
    assert_eq!(
        inspect.1["NetworkSettings"]["Ports"]["5432/tcp"][0]["HostIp"],
        "127.0.0.1"
    );

    let logs = request_text(
        &client,
        &socket,
        Method::GET,
        "/v1.41/containers/cid-1/logs?stdout=1&stderr=1",
        None,
    )
    .await;
    assert_eq!(logs.0, StatusCode::OK);

    let denied = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({
            "Image": "postgres:16",
            "HostConfig": {"Privileged": true}
        })),
        Some("sess-blocked"),
    )
    .await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    assert_eq!(denied.1["kind"], "policy_denied");
    assert_eq!(denied.1["rule_id"], "PSP-POL-001");

    let remove = request_text(
        &client,
        &socket,
        Method::DELETE,
        "/v1.41/containers/cid-1?force=1",
        None,
    )
    .await;
    assert_eq!(remove.0, StatusCode::NO_CONTENT);

    assert!(
        calls
            .snapshot()
            .iter()
            .any(|(_, path)| path.contains("/containers/create"))
    );

    let _ = psp_shutdown.send(());
    let _ = backend_shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend_handle.await;
}

#[tokio::test]
async fn compatibility_suite_handles_parallel_inspect_requests() {
    let calls = Calls::default();
    let (backend_url, backend_shutdown, backend_handle) =
        spawn_lifecycle_backend(calls.clone()).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle, _state) = spawn_psp(socket.clone(), backend_url, false).await;
    let client: TestClient<_, Full<Bytes>> =
        TestClient::builder(TokioExecutor::new()).build(UnixConnector);

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let socket = socket.clone();
        tasks.push(tokio::spawn(async move {
            request_json(
                &client,
                &socket,
                Method::GET,
                "/v1.41/containers/cid-1/json",
                None,
                None,
            )
            .await
        }));
    }

    for task in tasks {
        let (status, body) = task.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["NetworkSettings"]["Ports"]["5432/tcp"][0]["HostIp"],
            "127.0.0.1"
        );
    }

    let _ = psp_shutdown.send(());
    let _ = backend_shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend_handle.await;
}

#[tokio::test]
async fn compatibility_suite_cleans_up_tracked_resources() {
    let calls = Calls::default();
    let (backend_url, backend_shutdown, backend_handle) =
        spawn_lifecycle_backend(calls.clone()).await;
    let state = AppState::new(
        BackendConfig::Http(backend_url),
        test_policy(),
        "127.0.0.1",
        false,
    )
    .unwrap();
    let app = router(state.clone());

    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let client: TestClient<_, Full<Bytes>> =
        TestClient::builder(TokioExecutor::new()).build(UnixConnector);
    let _ = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create?name=test-a",
        Some(json!({"Image":"postgres:16"})),
        Some("sess-cleanup"),
    )
    .await;
    let _ = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create?name=test-b",
        Some(json!({"Image":"postgres:16"})),
        Some("sess-cleanup"),
    )
    .await;

    state.cleanup_tracked_resources().await.unwrap();

    let snapshot = calls.snapshot();
    assert!(
        snapshot
            .iter()
            .any(|(_, path)| path == "/containers/cid-1?force=1")
    );
    assert!(
        snapshot
            .iter()
            .any(|(_, path)| path == "/containers/cid-2?force=1")
    );

    let _ = shutdown_tx.send(());
    let _ = backend_shutdown.send(());
    let _ = server.await;
    let _ = backend_handle.await;
}

async fn spawn_psp(
    socket: std::path::PathBuf,
    backend_url: Url,
    keep_on_failure: bool,
) -> (oneshot::Sender<()>, JoinHandle<()>, AppState) {
    let state = AppState::new(
        BackendConfig::Http(backend_url),
        test_policy(),
        "127.0.0.1",
        keep_on_failure,
    )
    .unwrap();
    let app = router(state.clone());
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (tx, handle, state)
}

async fn spawn_lifecycle_backend(calls: Calls) -> (Url, oneshot::Sender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sequence = Arc::new(AtomicUsize::new(1));
    let app = Router::new().route(
        "/{*path}",
        any(move |req: Request| {
            let calls = calls.clone();
            let sequence = sequence.clone();
            async move {
                calls.push(req.method(), req.uri());
                let path = req.uri().path();
                let normalized = normalize_versioned_path(path);
                match (req.method().clone(), normalized.as_str()) {
                    (Method::POST, "/containers/create") => {
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let json: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(json["Labels"][LABEL_MANAGED], "true");
                        assert!(
                            json["Labels"][LABEL_SESSION]
                                .as_str()
                                .unwrap()
                                .starts_with("sess-")
                        );
                        let id = format!("cid-{}", sequence.fetch_add(1, Ordering::SeqCst));
                        (StatusCode::CREATED, Json(json!({"Id": id}))).into_response()
                    }
                    (Method::POST, path)
                        if path.starts_with("/containers/") && path.ends_with("/start") =>
                    {
                        StatusCode::NO_CONTENT.into_response()
                    }
                    (Method::GET, path)
                        if path.starts_with("/containers/") && path.ends_with("/json") =>
                    {
                        Json(json!({
                            "Id": "cid-1",
                            "NetworkSettings": {
                                "Ports": {
                                    "5432/tcp": [{"HostIp":"0.0.0.0","HostPort":"15432"}]
                                }
                            }
                        }))
                        .into_response()
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
                    (Method::DELETE, path) if path.starts_with("/containers/") => {
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

fn normalize_versioned_path(path: &str) -> String {
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

async fn request_json(
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

async fn request_text(
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
