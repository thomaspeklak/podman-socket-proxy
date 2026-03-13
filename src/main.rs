mod cli;

use std::io::IsTerminal;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    let stderr_layer = tracing_subscriber::fmt::layer()
        .without_time()
        .with_ansi(std::io::stderr().is_terminal());

    // PSP_LOG_FILE=/path/to/psp.log — appends structured logs to a file.
    // Useful when PSP is launched by an external process (e.g. AGS) where
    // stderr is not easily captured. RUST_LOG controls the level for both.
    let file_layer = std::env::var("PSP_LOG_FILE").ok().and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|file| {
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
            })
    });

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(stderr_layer)
        .with(file_layer)
        .init();

    if let Err(err) = cli::run().await {
        let is_tty = std::io::stderr().is_terminal();
        if is_tty {
            eprintln!("\x1b[1;31merror\x1b[0m: {err:#}");
        } else {
            eprintln!("error: {err:#}");
        }
        std::process::exit(1);
    }
}
