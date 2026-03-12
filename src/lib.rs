pub mod audit;
pub mod backend;
pub mod config;
pub mod error;
pub mod paths;
pub mod policy;
pub mod proxy;
pub mod rewrite;
pub mod session;

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use axum::{Router, routing::any};
use tokio::net::UnixListener;
use tracing::info;

pub use config::{BackendConfig, COMPATIBILITY_PROFILE, Config, ConfigSources, ResolvedConfig};
pub use error::ProxyError;
pub use paths::{is_supported_endpoint, normalize_versioned_path};
pub use policy::Policy;

use backend::{BackendClient, DiscoveredContainer};
use session::{LABEL_MANAGED, SessionManager};

#[derive(Clone)]
pub struct AppState {
    pub(crate) backend: BackendClient,
    pub(crate) policy: Policy,
    pub(crate) advertised_host: String,
    pub(crate) require_session_id: bool,
    pub sessions: SessionManager,
    request_sequence: Arc<AtomicU64>,
}

impl AppState {
    pub fn new(
        backend: BackendConfig,
        policy: Policy,
        advertised_host: impl Into<String>,
        keep_on_failure: bool,
        require_session_id: bool,
    ) -> Result<Self> {
        Ok(Self {
            backend: BackendClient::new(backend)?,
            policy,
            advertised_host: advertised_host.into(),
            require_session_id,
            sessions: SessionManager::new(keep_on_failure),
            request_sequence: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn next_request_id(&self) -> String {
        let next = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        format!("psp-{next:08x}")
    }

    pub async fn startup_sweep(&self) -> Result<(), ProxyError> {
        let filter = serde_json::json!({"label": [format!("{}=true", LABEL_MANAGED)]});
        let filter_encoded = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("all", "1")
            .append_pair("filters", &filter.to_string())
            .finish();
        let response = self
            .backend
            .get_json(&format!("/containers/json?{filter_encoded}"))
            .await?;

        let Some(containers) = response.as_array() else {
            return Ok(());
        };

        let count = containers.len();
        if count > 0 {
            info!(count, "startup sweep: removing orphaned psp-managed containers");
        }
        for container in containers {
            if let Some(id) = container.get("Id").and_then(|value| value.as_str()) {
                let name = container.get("Names")
                    .and_then(|n| n.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|n| n.as_str())
                    .unwrap_or(id);
                info!(container_id = %id, container_name = %name, "startup sweep: removing container");
                self.backend.delete(&format!("/containers/{id}?force=1")).await?;
            }
        }

        Ok(())
    }

    pub async fn cleanup_tracked_resources(&self) -> Result<(), ProxyError> {
        if self.sessions.keep_on_failure() {
            let ids = self.sessions.tracked_container_ids();
            if !ids.is_empty() {
                info!(count = ids.len(), "shutdown cleanup skipped: keep_on_failure=true, leaving containers");
            }
            return Ok(());
        }

        let ids = self.sessions.tracked_container_ids();
        if !ids.is_empty() {
            info!(count = ids.len(), "shutdown cleanup: removing psp-managed containers");
        }
        for id in ids {
            info!(container_id = %id, "shutdown cleanup: removing container");
            self.backend.delete(&format!("/containers/{id}?force=1")).await?;
        }

        Ok(())
    }

    pub async fn list_containers(&self, all: bool) -> Result<Vec<DiscoveredContainer>, ProxyError> {
        self.backend.list_containers(all).await
    }

    pub async fn backend_ping(&self) -> Result<(), ProxyError> {
        self.backend.ping().await
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", any(proxy::proxy_request))
        .route("/{*path}", any(proxy::proxy_request))
        .with_state(Arc::new(state))
}

pub async fn serve_with_shutdown(config: Config) -> Result<()> {
    if !config.policy_path.exists() {
        anyhow::bail!(
            "policy file not found: {path}\n\n  Create one with:\n    psp policy init {path}\n\n  Or run first-time setup:\n    psp init",
            path = config.policy_path.display()
        );
    }
    let policy = Policy::load(&config.policy_path).with_context(|| {
        format!(
            "failed to load policy from {}",
            config.policy_path.display()
        )
    })?;

    let state = AppState::new(
        config.backend.clone(),
        policy,
        config.advertised_host.clone(),
        config.keep_on_failure,
        config.require_session_id,
    )?;

    state
        .backend
        .ping()
        .await
        .map_err(|error| anyhow!("backend health check failed: {error:?}"))?;
    state
        .startup_sweep()
        .await
        .map_err(|error| anyhow!("startup sweep failed: {error:?}"))?;

    if let Some(parent) = config.listen_socket.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create socket dir {}", parent.display()))?;
    }

    remove_existing_socket(&config.listen_socket).await?;

    let listener = UnixListener::bind(&config.listen_socket)
        .with_context(|| format!("failed to bind socket {}", config.listen_socket.display()))?;

    info!(
        socket = %config.listen_socket.display(),
        backend = %config.backend.display_string(),
        policy = %config.policy_path.display(),
        advertised_host = %config.advertised_host,
        keep_on_failure = config.keep_on_failure,
        require_session_id = config.require_session_id,
        compatibility_profile = COMPATIBILITY_PROFILE,
        "psp ready"
    );

    // Human-friendly startup banner to stderr
    let socket_url = format!("unix://{}", config.listen_socket.display());
    eprintln!();
    eprintln!("  psp {} ({}) ready", env!("CARGO_PKG_VERSION"), COMPATIBILITY_PROFILE);
    eprintln!("  socket  : {socket_url}");
    eprintln!("  backend : {}", config.backend.display_string());
    eprintln!("  policy  : {}", config.policy_path.display());
    eprintln!();
    eprintln!("  export DOCKER_HOST={socket_url}");
    eprintln!();

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

pub async fn run_startup_checks(config: &Config) -> Result<()> {
    let _policy = Policy::load(&config.policy_path).with_context(|| {
        format!(
            "failed to load policy from {}",
            config.policy_path.display()
        )
    })?;
    let backend = BackendClient::new(config.backend.clone())?;
    backend
        .ping()
        .await
        .map_err(|error| anyhow!("backend health check failed: {error:?}"))?;
    let _ = backend
        .version()
        .await
        .map_err(|error| anyhow!("backend version probe failed: {error:?}"))?;
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
