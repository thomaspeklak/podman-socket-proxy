mod cli;

use std::io::IsTerminal;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .without_time()
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
