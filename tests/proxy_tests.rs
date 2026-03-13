mod helpers;

use std::sync::{Arc, Mutex};

use axum::http::{Method, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request as HyperRequest;
use podman_socket_proxy::{
    AppState, BackendConfig,
    error,
    is_supported_endpoint, normalize_versioned_path,
    policy,
    session,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use helpers::*;

#[tokio::test]
async fn proxies_container_lifecycle_endpoints() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let create = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create?name=test",
        Some(json!({"Image":"postgres:16"})),
        None,
    )
    .await;
    assert_eq!(create.0, StatusCode::CREATED);
    assert_eq!(create.1["Id"], "cid-1");

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
    assert_eq!(inspect.1["Id"], "cid-1");

    let logs = request_text(
        &client,
        &socket,
        Method::GET,
        "/v1.41/containers/cid-1/logs?stdout=1&stderr=1",
        None,
    )
    .await;
    assert_eq!(logs.0, StatusCode::OK);
    assert_eq!(logs.1, "ready\n");

    let wait = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/cid-1/wait",
        None,
        None,
    )
    .await;
    assert_eq!(wait.0, StatusCode::OK);
    assert_eq!(wait.1["StatusCode"], 0);

    let remove = request_text(
        &client,
        &socket,
        Method::DELETE,
        "/v1.41/containers/cid-1?force=1",
        None,
    )
    .await;
    assert_eq!(remove.0, StatusCode::NO_CONTENT);
    assert_eq!(remove.1, "");

    assert_eq!(
        calls.snapshot(),
        vec![
            ("POST".into(), "/v1.41/containers/create?name=test".into()),
            ("POST".into(), "/v1.41/containers/cid-1/start".into()),
            ("GET".into(), "/v1.41/containers/cid-1/json".into()),
            (
                "GET".into(),
                "/v1.41/containers/cid-1/logs?stdout=1&stderr=1".into()
            ),
            ("POST".into(), "/v1.41/containers/cid-1/wait".into()),
            ("DELETE".into(), "/v1.41/containers/cid-1?force=1".into()),
        ]
    );

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn rewrites_container_inspect_host_ports() {
    let calls = Calls::default();
    let inspect_body = json!({
        "Id": "cid-123",
        "Name": "/cid-123",
        "Config": {
            "Image": "postgres:16",
            "Labels": {
                "io.psp.managed": "true"
            }
        },
        "NetworkSettings": {
            "Ports": {
                "5432/tcp": [{"HostIp": "0.0.0.0", "HostPort": "15432"}],
                "8080/tcp": [{"HostIp": "0.0.0.0", "HostPort": "18080"}]
            }
        }
    });
    let backend =
        spawn_mock_backend(LifecycleMock::with_inspect_body(calls, inspect_body)).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let inspect = request_json(
        &client,
        &socket,
        Method::GET,
        "/v1.41/containers/cid-123/json",
        None,
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

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn injects_session_labels_on_container_create() {
    let captured = Arc::new(Mutex::new(None::<Value>));
    let backend = spawn_mock_backend(CaptureMock {
        captured: captured.clone(),
    })
    .await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let create = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({"Image":"postgres:16"})),
        Some("sess-123"),
    )
    .await;
    assert_eq!(create.0, StatusCode::CREATED);

    let captured = captured.lock().unwrap().clone().unwrap();
    assert_eq!(captured["Labels"][session::LABEL_MANAGED], "true");
    assert_eq!(captured["Labels"][session::LABEL_SESSION], "sess-123");

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn startup_sweep_removes_stale_managed_containers() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(SweepMock {
        calls: calls.clone(),
    })
    .await;
    let state = AppState::new(
        BackendConfig::Http(backend.url),
        test_policy(),
        "127.0.0.1",
        false,
        false,
    )
    .unwrap();
    state.startup_sweep().await.unwrap();

    let snapshot = calls.snapshot();
    assert!(snapshot.iter().any(|(m, p)| m == "GET" && p.starts_with("/containers/json?")));
    assert!(snapshot.iter().any(|(m, p)| m == "DELETE" && p == "/containers/stale-1?force=1"));

    let _ = backend.shutdown.send(());
    let _ = backend.handle.await;
}

#[tokio::test]
async fn cleanup_tracked_resources_deletes_containers_on_shutdown() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let state = AppState::new(
        BackendConfig::Http(backend.url),
        test_policy(),
        "127.0.0.1",
        false,
        false,
    )
    .unwrap();
    state.sessions.track_container("sess-1", "cid-123");
    state.cleanup_tracked_resources().await.unwrap();

    assert!(calls
        .snapshot()
        .iter()
        .any(|(m, p)| m == "DELETE" && p == "/containers/cid-123?force=1"));

    let _ = backend.shutdown.send(());
    let _ = backend.handle.await;
}

#[test]
fn derives_audit_context() {
    use podman_socket_proxy::audit::RequestAuditContext;

    let body_value = json!({"Image":"postgres:16","Env":["TOKEN=secret"]});
    let audit = RequestAuditContext::from_request(
        &Method::POST,
        "/containers/create",
        "/v1.41/containers/create",
        None,
        Some(&body_value),
        "sess-a".into(),
    );
    assert_eq!(audit.session_id, "sess-a");
    assert_eq!(audit.operation, "containers.create");
    assert_eq!(audit.target_image.as_deref(), Some("postgres:16"));
    assert_eq!(audit.target_container, None);
}

#[test]
fn accepts_versioned_and_unversioned_supported_paths() {
    let n = |p| normalize_versioned_path(p);
    assert!(is_supported_endpoint(&Method::GET, &n("/_ping")));
    assert!(is_supported_endpoint(&Method::GET, &n("/v1.41/_ping")));
    assert!(is_supported_endpoint(&Method::POST, &n("/containers/create")));
    assert!(is_supported_endpoint(
        &Method::POST,
        &n("/v1.41/containers/create")
    ));
    assert!(is_supported_endpoint(
        &Method::DELETE,
        &n("/v1.41/containers/cid-123")
    ));
    assert!(!is_supported_endpoint(
        &Method::POST,
        &n("/v1.41/networks/create")
    ));
}

#[tokio::test]
async fn denies_policy_blocked_container_create_with_rule_id() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let response = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({
            "Image": "postgres:16",
            "HostConfig": {"Privileged": true}
        })),
        None,
    )
    .await;
    assert_eq!(response.0, StatusCode::FORBIDDEN);
    assert_eq!(response.1["kind"], "policy_denied");
    assert_eq!(response.1["rule_id"], policy::RULE_PRIVILEGED);
    assert!(calls.snapshot().is_empty());

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn rejects_unsupported_endpoints_with_structured_error() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let response = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/networks/create",
        Some(json!({"Name":"n1"})),
        None,
    )
    .await;
    assert_eq!(response.0, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(response.1["kind"], "unsupported_endpoint");
    assert_eq!(response.1["path"], "/v1.41/networks/create");
    assert!(calls.snapshot().is_empty());

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[test]
fn rejects_deep_nested_container_paths() {
    // /containers/foo/bar/json should NOT match — only /containers/{id}/json
    assert!(!is_supported_endpoint(
        &Method::GET,
        "/containers/foo/bar/json"
    ));
    assert!(!is_supported_endpoint(
        &Method::POST,
        "/containers/foo/bar/start"
    ));
}

#[test]
fn accepts_image_inspect_paths() {
    // simple image name
    assert!(is_supported_endpoint(&Method::GET, "/images/postgres:16/json"));
    // namespaced image (org/name:tag) — name has a slash, so path has 4 segments
    assert!(is_supported_endpoint(
        &Method::GET,
        "/images/timescale/timescaledb:2.24.0-pg16/json"
    ));
    // versioned prefix is stripped before matching
    assert!(is_supported_endpoint(
        &Method::GET,
        &normalize_versioned_path("/v1.41/images/postgres:16/json")
    ));
    // /images/json (list) must NOT match
    assert!(!is_supported_endpoint(&Method::GET, "/images/json"));
    // wrong method
    assert!(!is_supported_endpoint(&Method::DELETE, "/images/postgres:16/json"));
    assert!(!is_supported_endpoint(&Method::POST, "/images/postgres:16/json"));
}

#[test]
fn normalize_strips_version_prefix() {
    assert_eq!(normalize_versioned_path("/v1.41/_ping"), "/_ping");
    assert_eq!(
        normalize_versioned_path("/v1.41/containers/create"),
        "/containers/create"
    );
    assert_eq!(normalize_versioned_path("/_ping"), "/_ping");
}

#[tokio::test]
async fn rejects_oversized_chunked_body() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    // Build a request with a body over 4 MiB but NO Content-Length header
    let oversized = vec![b'x'; 5 * 1024 * 1024]; // 5 MiB
    let request = HyperRequest::builder()
        .method(Method::POST)
        .uri::<hyper::Uri>(hyperlocal::Uri::new(&socket, "/v1.41/containers/create").into())
        .header(http::header::CONTENT_TYPE, "application/json")
        // Deliberately no Content-Length header
        .body(Full::new(Bytes::from(oversized)))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // Verify nothing reached the backend
    assert!(calls.snapshot().is_empty());

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn denies_host_userns_mode_via_proxy() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let response = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({
            "Image": "postgres:16",
            "HostConfig": {"UsernsMode": "host"}
        })),
        None,
    )
    .await;
    assert_eq!(response.0, StatusCode::FORBIDDEN);
    assert_eq!(response.1["kind"], "policy_denied");
    assert_eq!(response.1["rule_id"], policy::RULE_HOST_NAMESPACE);
    assert!(calls.snapshot().is_empty());

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn sanitizes_malicious_session_id_in_labels() {
    let captured = Arc::new(Mutex::new(None::<Value>));
    let backend = spawn_mock_backend(CaptureMock {
        captured: captured.clone(),
    })
    .await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    // Send a request with an invalid session ID containing spaces
    // (spaces are valid in HTTP headers but rejected by the session sanitizer)
    let create = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({"Image":"postgres:16"})),
        Some("evil session id"),
    )
    .await;
    assert_eq!(create.0, StatusCode::CREATED);

    // Verify the label was sanitized to "anonymous"
    let captured = captured.lock().unwrap().clone().unwrap();
    assert_eq!(captured["Labels"][session::LABEL_SESSION], "anonymous");

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn denies_non_managed_container_access_by_default() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let response = request_json(
        &client,
        &socket,
        Method::GET,
        "/v1.41/containers/existing-db/json",
        None,
        None,
    )
    .await;
    assert_eq!(response.0, StatusCode::FORBIDDEN);
    assert_eq!(response.1["rule_id"], policy::RULE_CONTAINER_ALLOWLIST);

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn returns_request_id_and_remediation_metadata_on_denials() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls)).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let response = raw_request(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({
            "Image": "postgres:16",
            "HostConfig": {"Privileged": true}
        })),
        Some("sess-deny"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().contains_key(error::REQUEST_ID_HEADER));
    assert_eq!(
        response
            .headers()
            .get(session::EFFECTIVE_SESSION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("sess-deny")
    );
    let request_id = response
        .headers()
        .get(error::REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["rule_id"], policy::RULE_PRIVILEGED);
    assert!(body["hint"].as_str().unwrap().contains("Privileged"));
    assert_eq!(body["request_id"].as_str(), Some(request_id.as_str()));

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn enforces_required_session_id_for_mutating_requests() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls.clone())).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp_with_opts(socket.clone(), backend.url, false, true).await;
    let client = new_test_client();

    let response = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({"Image":"postgres:16"})),
        None,
    )
    .await;
    assert_eq!(response.0, StatusCode::BAD_REQUEST);
    assert_eq!(response.1["kind"], "session_required");
    assert!(calls.snapshot().is_empty());

    let _ = psp_shutdown.send(());
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}
