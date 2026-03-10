use anyhow::Result;
use podman_socket_proxy::{Config, serve_with_shutdown};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
        .init();

    serve_with_shutdown(Config::from_env()?).await
}
