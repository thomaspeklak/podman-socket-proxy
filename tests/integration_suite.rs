mod helpers;

use axum::http::{Method, StatusCode};
use podman_socket_proxy::{AppState, BackendConfig, policy, router};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::sync::oneshot;

use helpers::*;

#[tokio::test]
async fn compatibility_suite_covers_happy_and_blocked_paths() {
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
    assert_eq!(denied.1["rule_id"], policy::RULE_PRIVILEGED);

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
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn compatibility_suite_handles_parallel_inspect_requests() {
    let calls = Calls::default();
    let backend = spawn_mock_backend(LifecycleMock::new(calls)).await;
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("psp.sock");
    let (psp_shutdown, psp_handle) = spawn_psp(socket.clone(), backend.url).await;
    let client = new_test_client();

    let create = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create?name=parallel-test",
        Some(json!({"Image":"postgres:16"})),
        Some("sess-parallel"),
    )
    .await;
    assert_eq!(create.0, StatusCode::CREATED);

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
    let _ = backend.shutdown.send(());
    let _ = psp_handle.await;
    let _ = backend.handle.await;
}

#[tokio::test]
async fn compatibility_suite_cleans_up_tracked_resources() {
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

    let client = new_test_client();
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
    let _ = backend.shutdown.send(());
    let _ = server.await;
    let _ = backend.handle.await;
}
