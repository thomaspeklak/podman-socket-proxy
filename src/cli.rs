use std::{
    fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use comfy_table::{Attribute, Cell, CellAlignment, Color, ColumnConstraint, ContentArrangement, Table, Width, presets};
use indicatif::{ProgressBar, ProgressStyle};
use axum::http::{Method, StatusCode};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::generate;
use dialoguer::{Input, MultiSelect, Select};
use http_body_util::{BodyExt, Full};
use hyper::Request as HyperRequest;
use hyper_util::{client::legacy::Client as TestClient, rt::TokioExecutor};
use hyperlocal::UnixConnector;
use podman_socket_proxy::{
    AppState, COMPATIBILITY_PROFILE, Config, Policy, ResolvedConfig, normalize_versioned_path,
    router, run_startup_checks, serve_with_shutdown,
    session::{EFFECTIVE_SESSION_HEADER, SESSION_HEADER},
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    net::UnixListener,
    sync::oneshot,
    task::JoinHandle,
};

// ── Color helpers ────────────────────────────────────────────────────────────

fn col_ok(s: &str) -> String {
    if io::stdout().is_terminal() { format!("\x1b[32m{s}\x1b[0m") } else { s.to_string() }
}
fn col_err(s: &str) -> String {
    if io::stdout().is_terminal() { format!("\x1b[31m{s}\x1b[0m") } else { s.to_string() }
}
fn col_warn(s: &str) -> String {
    if io::stdout().is_terminal() { format!("\x1b[33m{s}\x1b[0m") } else { s.to_string() }
}
fn col_bold(s: &str) -> String {
    if io::stdout().is_terminal() { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() }
}
fn col_dim(s: &str) -> String {
    if io::stdout().is_terminal() { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() }
}

// ── TUI helpers ──────────────────────────────────────────────────────────────

/// Detect terminal width, respecting the COLUMNS env var override for tests/CI.
fn term_width() -> u16 {
    if let Ok(s) = std::env::var("COLUMNS") {
        if let Ok(w) = s.parse::<u16>() {
            return w;
        }
    }
    120
}

/// Strip ANSI escape sequences and return the visible character length.
fn visible_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape && c == 'm' {
            in_escape = false;
        } else if !in_escape {
            len += 1;
        }
    }
    len
}

/// Create a styled table for TTY output. Returns None when stdout is not a terminal.
fn make_table(headers: &[&str]) -> Option<Table> {
    if !io::stdout().is_terminal() {
        return None;
    }
    let mut table = Table::new();
    table.load_preset(presets::UTF8_FULL);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_width(term_width());
    table.set_header(
        headers.iter().map(|h| Cell::new(h).add_attribute(Attribute::Bold))
    );
    Some(table)
}

/// Create a spinner on stderr. Hidden when not a TTY.
fn make_spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    if io::stderr().is_terminal() {
        pb.set_style(
            ProgressStyle::with_template("  {spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.enable_steady_tick(Duration::from_millis(80));
    } else {
        pb.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }
    pb.set_message(msg.into());
    pb
}

fn star_rating(count: u64) -> String {
    let filled = match count {
        n if n >= 10_000 => 5,
        n if n >= 1_000 => 4,
        n if n >= 100 => 3,
        n if n >= 10 => 2,
        n if n >= 1 => 1,
        _ => 0,
    };
    format!("{}{}", "★".repeat(filled), "☆".repeat(5 - filled))
}

fn access_and_state_cells(access: &str, state_str: &str, is_running: bool) -> (Cell, Cell) {
    let access_cell = match access {
        "allowed"      => Cell::new("● ALLOWED").fg(Color::Green),
        "denied"       => Cell::new("✕ DENIED").fg(Color::Red),
        "default-deny" => Cell::new("○ DEFAULT").fg(Color::Yellow),
        "managed"      => Cell::new("◆ MANAGED").fg(Color::Blue),
        other          => Cell::new(other),
    };
    let state_cell = if is_running {
        Cell::new(state_str).fg(Color::Green)
    } else {
        Cell::new(state_str).fg(Color::DarkGrey)
    };
    (access_cell, state_cell)
}

// ── CLI definition ───────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "psp", about = "Podman Socket Proxy", version)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the PSP proxy server
    Run,
    /// Validate backend, policy, and resolved config
    Doctor,
    /// Inspect effective configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Policy authoring and diagnostics
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Discover existing backend containers and manage access policy
    Discover {
        #[command(subcommand)]
        command: Option<DiscoverCommand>,
    },
    /// Search for images and update allow policy
    Images {
        #[command(subcommand)]
        command: ImagesCommand,
    },
    /// Run a local PSP smoke test against the configured backend
    SmokeTest(SmokeTestArgs),
    /// Print version/build information
    Version,
    /// Check if PSP is running and show connection info
    Status,
    /// List containers currently managed by PSP
    Ps(PsArgs),
    /// Print integration environment variables for shells
    Env,
    /// Remove all PSP-managed containers
    Cleanup(CleanupArgs),
    /// Generate shell completion scripts
    Completions(CompletionsArgs),
    /// First-run setup: scaffold config and policy files
    Init(InitArgs),
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    Show,
}

#[derive(Subcommand, Debug)]
enum PolicyCommand {
    Check(CheckPolicyArgs),
    Explain(ExplainPolicyArgs),
    Init(InitPolicyArgs),
    /// Compare two policy files and show what changed
    Diff(DiffPolicyArgs),
}

#[derive(Subcommand, Debug)]
enum DiscoverCommand {
    Containers(ListContainersArgs),
    Allow(AllowContainerArgs),
    Deny(DenyContainerArgs),
}

#[derive(Subcommand, Debug)]
enum ImagesCommand {
    Search(SearchImagesArgs),
    Allow(AllowImageArgs),
}

#[derive(Args, Debug)]
struct CheckPolicyArgs {
    file: PathBuf,
}

#[derive(Args, Debug)]
struct ExplainPolicyArgs {
    #[arg(long)]
    policy: Option<PathBuf>,
    #[arg(long, default_value = "POST")]
    method: String,
    #[arg(long, default_value = "/containers/create")]
    path: String,
    #[arg(long)]
    query: Option<String>,
    #[arg(long = "request-file")]
    request_file: Option<PathBuf>,
}

#[derive(Clone, Debug, ValueEnum)]
enum InitProfile {
    Minimal,
    WorkspacePostgres,
    DebugWorkspace,
}

#[derive(Args, Debug)]
struct InitPolicyArgs {
    output: PathBuf,
    #[arg(long, value_enum)]
    profile: Option<InitProfile>,
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct ListContainersArgs {
    #[arg(long)]
    running: bool,
    #[arg(long)]
    stopped: bool,
}

#[derive(Args, Debug)]
struct AllowContainerArgs {
    container: Option<String>,
    #[arg(long, conflicts_with = "policy")]
    project: bool,
    #[arg(long)]
    policy: Option<PathBuf>,
    #[arg(long, help = "Show what would change without modifying the policy file")]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct DenyContainerArgs {
    container: Option<String>,
    #[arg(long, conflicts_with = "policy")]
    project: bool,
    #[arg(long)]
    policy: Option<PathBuf>,
    #[arg(long, help = "Show what would change without modifying the policy file")]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct SearchImagesArgs {
    query: String,
    #[arg(long, default_value_t = 10)]
    limit: usize,
}

#[derive(Args, Debug)]
struct AllowImageArgs {
    /// Image to add to the allowlist (omit for interactive search)
    image: Option<String>,
    #[arg(long)]
    policy: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct SmokeTestArgs {
    #[arg(long)]
    image: Option<String>,
}

#[derive(Args, Debug)]
struct PsArgs {
    #[arg(long, help = "Show only running containers")]
    running: bool,
}

#[derive(Args, Debug)]
struct CleanupArgs {
    #[arg(long, help = "Skip confirmation prompt")]
    force: bool,
    #[arg(long, help = "Only remove containers from this session")]
    session: Option<String>,
}

#[derive(Clone, Debug, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Args, Debug)]
struct CompletionsArgs {
    #[arg(value_enum)]
    shell: CompletionShell,
}

#[derive(Args, Debug)]
struct InitArgs {
    #[arg(long, help = "Create .psp.json and policy in the current project instead of global config")]
    project: bool,
    #[arg(long, value_enum)]
    profile: Option<InitProfile>,
    #[arg(long, help = "Overwrite existing files")]
    force: bool,
}

#[derive(Args, Debug)]
struct DiffPolicyArgs {
    /// First policy file (baseline)
    a: PathBuf,
    /// Second policy file (comparison)
    b: PathBuf,
}

// ── Entry point ──────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            if io::stderr().is_terminal() {
                // Interactive: show help instead of silently starting the server
                Cli::command().print_long_help()?;
                println!();
                Ok(())
            } else {
                // Non-interactive (systemd units, scripts): start the server
                let resolved = Config::resolve_from_env()?;
                serve_with_shutdown(resolved.config).await
            }
        }
        Some(Command::Run) => {
            let resolved = Config::resolve_from_env()?;
            serve_with_shutdown(resolved.config).await
        }
        Some(Command::Doctor) => doctor().await,
        Some(Command::Config { command }) => match command {
            ConfigCommand::Show => show_config().await,
        },
        Some(Command::Policy { command }) => match command {
            PolicyCommand::Check(args) => check_policy(args),
            PolicyCommand::Explain(args) => explain_policy(args).await,
            PolicyCommand::Init(args) => init_policy(args),
            PolicyCommand::Diff(args) => diff_policy(args),
        },
        Some(Command::Discover { command }) => match command {
            None => discover_browser().await,
            Some(DiscoverCommand::Containers(args)) => discover_containers(args).await,
            Some(DiscoverCommand::Allow(args)) => {
                mutate_container_access(args.container, args.project, args.policy, true, args.dry_run).await
            }
            Some(DiscoverCommand::Deny(args)) => {
                mutate_container_access(args.container, args.project, args.policy, false, args.dry_run).await
            }
        },
        Some(Command::Images { command }) => match command {
            ImagesCommand::Search(args) => search_images(args).await,
            ImagesCommand::Allow(args) => allow_image(args).await,
        },
        Some(Command::SmokeTest(args)) => smoke_test(args).await,
        Some(Command::Version) => {
            println!("psp {} ({})", env!("CARGO_PKG_VERSION"), COMPATIBILITY_PROFILE);
            Ok(())
        }
        Some(Command::Status) => status().await,
        Some(Command::Ps(args)) => ps(args).await,
        Some(Command::Env) => env_cmd(),
        Some(Command::Cleanup(args)) => cleanup(args).await,
        Some(Command::Completions(args)) => completions(args),
        Some(Command::Init(args)) => psp_init(args).await,
    }
}

// ── section helper ────────────────────────────────────────────────────────────

fn section(label: &str) {
    if io::stdout().is_terminal() {
        let bar_len = 40usize.saturating_sub(label.len() + 3);
        println!("\n  {} {} {}", col_bold("──"), col_bold(label), col_dim(&"─".repeat(bar_len)));
    } else {
        println!("\n[{}]", label);
    }
}

// ── doctor ───────────────────────────────────────────────────────────────────

async fn doctor() -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    let config = &resolved.config;

    // Check policy file existence with a helpful hint
    if !config.policy_path.exists() {
        bail!(
            "policy file not found: {}\n\n  Create one with:\n    psp policy init {}\n  or run:\n    psp init",
            config.policy_path.display(),
            config.policy_path.display()
        );
    }
    let policy = Policy::load(&config.policy_path)?;

    // Backend check with spinner and actionable hints
    let backend_spinner = make_spinner("Checking backend connectivity…");
    let backend_result = run_startup_checks(config).await;
    backend_spinner.finish_and_clear();
    if let Err(err) = backend_result {
        let hint = match &config.backend {
            podman_socket_proxy::BackendConfig::Unix(path) => {
                if path.exists() {
                    "Backend socket exists but is not responding.\n  Try: podman info"
                } else {
                    "Backend socket not found.\n  Enable Podman socket: systemctl --user enable --now podman.socket\n  Or set PSP_BACKEND to point to your Podman socket."
                }
            }
            podman_socket_proxy::BackendConfig::Http(_) => {
                "Backend HTTP endpoint is not reachable.\n  Check that the backend is running and the URL is correct."
            }
        };
        bail!("{err:#}\n\n  {hint}");
    }

    // Create socket dir if needed for the check
    if let Some(parent) = config.listen_socket.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to prepare socket directory {}", parent.display()))?;
    }

    let socket_url = format!("unix://{}", config.listen_socket.display());

    println!("{}", col_bold(&format!("{} PSP doctor OK", col_ok("✓"))));

    section("Backend");
    println!("  {} backend:      {}", col_ok("✓"), config.backend.display_string());
    println!("  {} listen socket:{}", col_ok("✓"), format!("  {}", config.listen_socket.display()));
    println!("  {} policy:       {}", col_ok("✓"), config.policy_path.display());

    section("Config");
    println!("  {} advertised host:    {}", col_ok("·"), config.advertised_host);
    println!("  {} keep on failure:    {}", col_ok("·"), config.keep_on_failure);
    println!("  {} require session id: {}", col_ok("·"), config.require_session_id);
    println!("  {} compatibility:      {}", col_ok("·"), COMPATIBILITY_PROFILE);

    section("Policy");
    println!("  {} image allowlist:     {} entries", col_ok("·"), policy.images.allowlist.len());
    println!("  {} container allowlist: {} entries", col_ok("·"), policy.containers.allowlist.len());

    section("Config Sources");
    match &resolved.sources.global_config_path {
        Some(path) => {
            if resolved.sources.loaded_global_config.is_some() {
                println!("  global:  {} {}", path.display(), col_ok("[loaded]"));
            } else {
                println!("  global:  {} {}", path.display(), col_dim("[not found]"));
            }
        }
        None => {
            println!("  global:  {} {}", col_dim("(no XDG_CONFIG_HOME or HOME set)"), col_dim("[skipped]"));
        }
    }
    match &resolved.sources.project_config_path {
        Some(path) => {
            if resolved.sources.loaded_project_config.is_some() {
                println!("  project: {} {}", path.display(), col_ok("[loaded]"));
            } else {
                println!("  project: {} {}", path.display(), col_dim("[not found]"));
            }
        }
        None => {
            println!("  project: {}", col_dim("(not in a git repository)"));
        }
    }
    if let Some(root) = &resolved.sources.project_root {
        println!("  project root: {}", root.display());
    }

    section("Integration");
    println!("  export DOCKER_HOST={socket_url}");
    println!("  {}  # run: eval $(psp env)", col_dim("# or:"));

    Ok(())
}

// ── show_config ──────────────────────────────────────────────────────────────

async fn show_config() -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    println!("{}", serde_json::to_string_pretty(&resolved)?);
    Ok(())
}

// ── check_policy ─────────────────────────────────────────────────────────────

fn check_policy(args: CheckPolicyArgs) -> Result<()> {
    let policy = Policy::load(&args.file)?;
    println!("Policy OK: {}", args.file.display());
    println!("- version: {}", policy.version);
    println!("- bind allowlist: {}", policy.bind_mounts.allowlist.len());
    for (raw, normalized) in policy
        .bind_mounts
        .allowlist
        .iter()
        .zip(policy.bind_mounts.normalized_allowlist.iter())
    {
        println!("  - {} -> {}", raw, normalized);
    }
    println!("- image allowlist: {}", policy.images.allowlist.len());
    for (raw, normalized) in policy
        .images
        .allowlist
        .iter()
        .zip(policy.images.normalized_allowlist.iter())
    {
        println!("  - {} -> {}", raw, normalized);
    }
    println!("- image denylist: {}", policy.images.denylist.len());
    for (raw, normalized) in policy
        .images
        .denylist
        .iter()
        .zip(policy.images.normalized_denylist.iter())
    {
        println!("  - {} -> {}", raw, normalized);
    }
    println!("- container allowlist: {}", policy.containers.allowlist.len());
    for (raw, normalized) in policy
        .containers
        .allowlist
        .iter()
        .zip(policy.containers.normalized_allowlist.iter())
    {
        println!("  - {} -> {}", raw, normalized);
    }
    println!("- container denylist: {}", policy.containers.denylist.len());
    for (raw, normalized) in policy
        .containers
        .denylist
        .iter()
        .zip(policy.containers.normalized_denylist.iter())
    {
        println!("  - {} -> {}", raw, normalized);
    }

    if policy.bind_mounts.allowlist.iter().any(|entry| entry == "/") {
        println!("warning: bind allowlist contains / which effectively allows all bind mounts");
    }
    for entry in &policy.images.allowlist {
        if !entry.contains('/') || (!entry.split('/').next().unwrap_or_default().contains('.') && !entry.split('/').next().unwrap_or_default().contains(':') && entry.matches('/').count() < 2) {
            println!("warning: image allowlist entry {} relies on short-name normalization", entry);
        }
    }

    Ok(())
}

// ── explain_policy ────────────────────────────────────────────────────────────

async fn explain_policy(args: ExplainPolicyArgs) -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    let policy_path = args.policy.unwrap_or_else(|| resolved.config.policy_path.clone());
    let policy = Policy::load(&policy_path)?;

    let method = args.method.parse::<Method>()?;
    let normalized_path = normalize_versioned_path(&args.path);
    let body = if let Some(path) = args.request_file {
        fs::read(&path).with_context(|| format!("failed to read request file {}", path.display()))?
    } else {
        Vec::new()
    };

    println!("Policy:  {}", policy_path.display());
    println!("Request: {} {}", method, args.path);
    if normalized_path != args.path {
        println!("         {} (normalized)", col_dim(&normalized_path));
    }
    println!();

    match policy.evaluate_request(&method, &normalized_path, args.query.as_deref(), &body) {
        Ok(()) => {
            println!("{}", col_ok("ALLOW"));
            println!();
            println!("  All policy rules passed for this request.");
        }
        Err(denial) => {
            println!("{} {}", col_err("DENY"), col_bold(denial.rule_id));
            println!();
            println!("  Reason:  {}", denial.reason);
            let (hint, docs) = denial_hint_and_docs(denial.rule_id);
            if let Some(hint) = hint {
                println!("  Hint:    {}", hint);
            }
            if let Some(docs) = docs {
                println!("  Docs:    {}", col_dim(docs));
            }
            println!();
            println!("  To simulate a fix, edit the policy and re-run psp policy explain.");
        }
    }

    Ok(())
}

fn denial_hint_and_docs(rule_id: &str) -> (Option<&'static str>, Option<&'static str>) {
    match rule_id {
        "PSP-POL-000" => (Some("Send a valid Docker-compatible container create JSON payload."), Some("docs/examples/http-api-examples.md")),
        "PSP-POL-001" => (Some("Remove HostConfig.Privileged or update policy if this is intentional."), Some("docs/policy-reference.md")),
        "PSP-POL-002" => (Some("Use isolated namespaces instead of host namespace joins."), Some("docs/policy-reference.md")),
        "PSP-POL-003" => (Some("Allowlist only the narrow host path required for this bind mount."), Some("docs/policy-reference.md")),
        "PSP-POL-004" => (Some("Remove device mappings unless explicitly required."), Some("docs/policy-reference.md")),
        "PSP-POL-005" => (Some("Drop CapAdd entries or extend policy only after review."), Some("docs/policy-reference.md")),
        "PSP-POL-006" => (Some("Choose a different image or remove the denylist entry intentionally."), Some("docs/policy-reference.md")),
        "PSP-POL-007" => (Some("Add the image to the allowlist: psp images allow <image>"), Some("docs/policy-reference.md")),
        "PSP-POL-008" => (Some("Remove the container deny entry if access is intentionally approved."), Some("docs/policy-reference.md")),
        "PSP-POL-009" => (Some("Use discovery mode: psp discover allow <container>"), Some("docs/policy-reference.md")),
        _ => (None, Some("docs/policy-reference.md")),
    }
}

// ── resolve_profile ───────────────────────────────────────────────────────────

fn resolve_profile(profile: Option<InitProfile>) -> Result<InitProfile> {
    if let Some(p) = profile {
        return Ok(p);
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(InitProfile::Minimal);
    }
    let items = [
        "minimal          — empty allowlists, deny-by-default",
        "workspace-postgres — allows /workspace bind mount + postgres:16 image",
        "debug-workspace  — workspace-postgres + redis:7, with alpine denylist example",
    ];
    let selection = Select::new()
        .with_prompt("Choose a policy profile")
        .items(&items)
        .default(0)
        .interact()?;
    Ok(match selection {
        0 => InitProfile::Minimal,
        1 => InitProfile::WorkspacePostgres,
        _ => InitProfile::DebugWorkspace,
    })
}

// ── init_policy ───────────────────────────────────────────────────────────────

fn init_policy(args: InitPolicyArgs) -> Result<()> {
    if args.output.exists() && !args.force {
        bail!("refusing to overwrite existing file {}; pass --force to replace it", args.output.display());
    }

    let profile = resolve_profile(args.profile)?;
    let content = match profile {
        InitProfile::Minimal => {
            json!({
                "version": "v1",
                "bind_mounts": { "allowlist": [] },
                "images": { "allowlist": [], "denylist": [] },
                "containers": { "allowlist": [], "denylist": [] }
            })
        }
        InitProfile::WorkspacePostgres => {
            json!({
                "version": "v1",
                "bind_mounts": { "allowlist": ["/workspace"] },
                "images": { "allowlist": ["postgres:16"], "denylist": [] },
                "containers": { "allowlist": [], "denylist": [] }
            })
        }
        InitProfile::DebugWorkspace => {
            json!({
                "version": "v1",
                "bind_mounts": { "allowlist": ["/workspace"] },
                "images": { "allowlist": ["postgres:16", "redis:7"], "denylist": ["alpine:latest"] },
                "containers": { "allowlist": [], "denylist": [] }
            })
        }
    };

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.output, format!("{}\n", serde_json::to_string_pretty(&content)?))?;
    println!("Wrote policy template to {}", args.output.display());
    Ok(())
}

// ── diff_policy ───────────────────────────────────────────────────────────────

fn diff_policy(args: DiffPolicyArgs) -> Result<()> {
    let a = Policy::load(&args.a)?;
    let b = Policy::load(&args.b)?;

    println!("Comparing policy files:");
    println!("  A: {} ({})", args.a.display(), a.version);
    println!("  B: {} ({})", args.b.display(), b.version);
    println!();

    print_list_diff("bind_mounts.allowlist", &a.bind_mounts.allowlist, &b.bind_mounts.allowlist);
    print_list_diff("images.allowlist", &a.images.allowlist, &b.images.allowlist);
    print_list_diff("images.denylist", &a.images.denylist, &b.images.denylist);
    print_list_diff("containers.allowlist", &a.containers.allowlist, &b.containers.allowlist);
    print_list_diff("containers.denylist", &a.containers.denylist, &b.containers.denylist);

    Ok(())
}

fn print_list_diff(label: &str, a: &[String], b: &[String]) {
    use std::collections::HashSet;
    let a_set: HashSet<&String> = a.iter().collect();
    let b_set: HashSet<&String> = b.iter().collect();

    let mut added: Vec<&&String> = b_set.difference(&a_set).collect();
    let mut removed: Vec<&&String> = a_set.difference(&b_set).collect();
    added.sort();
    removed.sort();

    if added.is_empty() && removed.is_empty() {
        println!("{}  {}", col_bold(label), col_dim("(unchanged)"));
    } else {
        println!("{}:", col_bold(label));
        for entry in removed {
            println!("  {} {}", col_err("-"), entry);
        }
        for entry in added {
            println!("  {} {}", col_ok("+"), entry);
        }
    }
    println!();
}

// ── discover_browser ──────────────────────────────────────────────────────────

async fn discover_browser() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        // Non-interactive: fall back to container list
        return discover_containers(ListContainersArgs { running: false, stopped: false }).await;
    }

    loop {
        let spinner = make_spinner("Fetching containers…");
        let resolved = Config::resolve_from_env()?;
        let policy = Policy::load(&resolved.config.policy_path)?;
        let state = build_state(&resolved)?;
        let mut containers = state
            .list_containers(true)
            .await
            .map_err(|error| anyhow!("failed to list containers: {error:?}"))?;
        spinner.finish_and_clear();

        containers.sort_by(|a, b| a.metadata.display_name().cmp(&b.metadata.display_name()));

        if containers.is_empty() {
            println!("No containers found.");
            return Ok(());
        }

        if let Some(mut table) = make_table(&["ID", "NAME", "IMAGE", "STATE", "ACCESS"]) {
            table.column_mut(2).unwrap().set_constraint(ColumnConstraint::UpperBoundary(Width::Fixed(40)));
            for container in &containers {
                let access = container_access_label(&policy, &container.metadata);
                let is_running = container.state.as_deref() == Some("running");
                let state_str = container.state.as_deref()
                    .unwrap_or(container.status.as_deref().unwrap_or("unknown"));
                let (access_cell, state_cell) = access_and_state_cells(access, state_str, is_running);
                table.add_row(vec![
                    Cell::new(short_id(&container.metadata.id)).fg(Color::DarkGrey),
                    Cell::new(container.metadata.display_name()),
                    Cell::new(container.metadata.image.as_deref().unwrap_or("")).fg(Color::DarkCyan),
                    state_cell,
                    access_cell,
                ]);
            }
            println!("{table}");
        }
        println!();

        // Action menu
        let actions = ["Allow a container…", "Deny a container…", "Refresh", "Exit"];
        let selection = Select::new()
            .with_prompt("Action")
            .items(&actions)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                let resolved = Config::resolve_from_env()?;
                let policy_path = global_policy_path(&resolved)?;
                mutate_container_access(None, false, Some(policy_path), true, false).await?;
            }
            1 => {
                let resolved = Config::resolve_from_env()?;
                let policy_path = global_policy_path(&resolved)?;
                mutate_container_access(None, false, Some(policy_path), false, false).await?;
            }
            2 => {
                // Refresh — loop continues
                continue;
            }
            _ => break,
        }
    }

    Ok(())
}

// ── discover_containers ───────────────────────────────────────────────────────

async fn discover_containers(args: ListContainersArgs) -> Result<()> {
    let spinner = make_spinner("Fetching containers…");

    let resolved = Config::resolve_from_env()?;
    let policy = Policy::load(&resolved.config.policy_path)?;
    let state = build_state(&resolved)?;
    let mut containers = state
        .list_containers(true)
        .await
        .map_err(|error| anyhow!("failed to list containers: {error:?}"))?;

    spinner.finish_and_clear();

    containers.sort_by(|a, b| a.metadata.display_name().cmp(&b.metadata.display_name()));

    let filtered: Vec<_> = containers.iter().filter(|c| {
        let is_running = c.state.as_deref() == Some("running");
        if args.running && !args.stopped && !is_running { return false; }
        if args.stopped && !args.running && is_running { return false; }
        true
    }).collect();

    if filtered.is_empty() {
        println!("No containers found.");
        return Ok(());
    }

    if let Some(mut table) = make_table(&["ID", "NAME", "IMAGE", "STATE", "ACCESS"]) {
        table.column_mut(2).unwrap().set_constraint(ColumnConstraint::UpperBoundary(Width::Fixed(40)));
        for container in &filtered {
            let access = container_access_label(&policy, &container.metadata);
            let is_running = container.state.as_deref() == Some("running");
            let state_str = container.state.as_deref()
                .unwrap_or(container.status.as_deref().unwrap_or("unknown"));

            let (access_cell, state_cell) = access_and_state_cells(access, state_str, is_running);

            table.add_row(vec![
                Cell::new(short_id(&container.metadata.id)).fg(Color::DarkGrey),
                Cell::new(container.metadata.display_name()),
                Cell::new(container.metadata.image.as_deref().unwrap_or("")).fg(Color::DarkCyan),
                state_cell,
                access_cell,
            ]);
        }
        println!("{table}");
    } else {
        // Plain fallback for non-TTY
        println!("ID\tNAME\tIMAGE\tSTATE\tACCESS");
        for container in &filtered {
            let access = container_access_label(&policy, &container.metadata);
            let state_str = container.state.as_deref()
                .unwrap_or(container.status.as_deref().unwrap_or("unknown"));
            println!(
                "{}\t{}\t{}\t{}\t{}",
                short_id(&container.metadata.id),
                container.metadata.display_name(),
                container.metadata.image.as_deref().unwrap_or(""),
                state_str,
                access
            );
        }
    }

    Ok(())
}

// ── mutate_container_access ───────────────────────────────────────────────────

async fn mutate_container_access(
    container_ref: Option<String>,
    project: bool,
    policy_override: Option<PathBuf>,
    allow: bool,
    dry_run: bool,
) -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    let policy_path = discovery_policy_path(&resolved, policy_override, project)?;

    let spinner = make_spinner("Fetching containers…");
    let state = build_state(&resolved)?;
    let containers = state
        .list_containers(true)
        .await
        .map_err(|error| anyhow!("failed to list containers: {error:?}"))?;
    spinner.finish_and_clear();

    let mut policy = Policy::load(&policy_path)?;
    policy.validate()?;

    let targets = if let Some(container_ref) = container_ref {
        vec![find_container(&containers, &container_ref)?]
    } else {
        select_containers_interactively(&containers, &policy, allow)?
    };

    if targets.is_empty() {
        println!("No containers selected; policy unchanged.");
        return Ok(());
    }

    // Show preview box
    let action_label = if allow { col_ok("+ ALLOW") } else { col_err("- DENY") };

    println!();
    println!("  Changes to: {}", col_bold(&policy_path.display().to_string()));
    for target in &targets {
        println!("  {}  {} {}", action_label, target.metadata.display_name(), col_dim(&format!("({})", short_id(&target.metadata.id))));
        println!("  {}  {} {}", col_dim("  (by id)"), col_dim(&target.metadata.id[..target.metadata.id.len().min(12)]), col_dim(""));
    }
    println!();

    if dry_run {
        println!("{}", col_warn("Dry run — no changes written."));
        return Ok(());
    }

    if io::stdin().is_terminal() {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("Apply {} change(s)?", targets.len()))
            .default(true)
            .interact()?;
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    for target in &targets {
        policy_entry_mutation(&mut policy, &target.metadata, allow);
    }
    policy.save(&policy_path)?;

    println!("{} {} {} container(s) in {}",
        col_ok("✓"),
        if allow { "Allowed" } else { "Denied" },
        targets.len(),
        policy_path.display()
    );
    Ok(())
}

fn policy_entry_mutation(
    policy: &mut Policy,
    metadata: &podman_socket_proxy::policy::ContainerMetadata,
    allow: bool,
) {
    let primary = metadata.display_name();
    if allow {
        policy.add_container_allow(&metadata.id);
        policy.add_container_allow(&primary);
    } else {
        policy.add_container_deny(&metadata.id);
        policy.add_container_deny(&primary);
    }
}

fn select_containers_interactively<'a>(
    containers: &'a [podman_socket_proxy::backend::DiscoveredContainer],
    policy: &Policy,
    allow: bool,
) -> Result<Vec<&'a podman_socket_proxy::backend::DiscoveredContainer>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let action = if allow { "allow" } else { "deny" };
        bail!(
            "interactive discovery requires a terminal; rerun in a TTY or pass an explicit container name/id to `psp discover {action} <container>`"
        );
    }

    let candidates = interactive_candidates(containers, policy, allow);
    if candidates.is_empty() {
        let action = if allow { "allow" } else { "deny" };
        println!("No containers available to {action} — all are already in the correct state.");
        return Ok(Vec::new());
    }

    // Header showing counts
    let total = candidates.len();
    println!("{} container(s) available:", col_bold(&total.to_string()));
    println!();

    let items: Vec<String> = candidates.iter().map(|candidate| candidate.label.clone()).collect();
    let prompt = if allow {
        "Select containers to ALLOW (space to toggle, enter to confirm)"
    } else {
        "Select containers to DENY (space to toggle, enter to confirm)"
    };
    let selected = MultiSelect::new()
        .with_prompt(prompt)
        .items(&items)
        .interact()?;

    Ok(selected
        .into_iter()
        .map(|index| candidates[index].container)
        .collect())
}

fn interactive_candidates<'a>(
    containers: &'a [podman_socket_proxy::backend::DiscoveredContainer],
    policy: &Policy,
    allow: bool,
) -> Vec<InteractiveCandidate<'a>> {
    let mut candidates = Vec::new();
    for container in containers {
        if container.metadata.managed {
            continue;
        }

        let access = container_access_label(policy, &container.metadata);
        let include = if allow {
            access != "allowed"
        } else {
            access != "denied"
        };
        if !include {
            continue;
        }

        candidates.push(InteractiveCandidate {
            label: format_interactive_container_label(container, access),
            container,
        });
    }
    candidates.sort_by(|a, b| a.label.cmp(&b.label));
    candidates
}

fn format_interactive_container_label(
    container: &podman_socket_proxy::backend::DiscoveredContainer,
    access: &str,
) -> String {
    let badge = match access {
        "allowed"      => "● ALLOWED",
        "denied"       => "✕ DENIED",
        "default-deny" => "○ DEFAULT",
        "managed"      => "◆ MANAGED",
        other          => other,
    };
    let state_str = container.state.as_deref()
        .unwrap_or(container.status.as_deref().unwrap_or("?"));
    format!(
        "{:<12}  {:<28}  {:<22}  {}",
        badge,
        container.metadata.display_name(),
        container.metadata.image.as_deref().unwrap_or(""),
        state_str,
    )
}

fn container_access_label(policy: &Policy, metadata: &podman_socket_proxy::policy::ContainerMetadata) -> &'static str {
    if metadata.managed {
        "managed"
    } else if policy.containers.matches_deny(metadata) {
        "denied"
    } else if policy.containers.matches_allow(metadata) {
        "allowed"
    } else {
        "default-deny"
    }
}

struct InteractiveCandidate<'a> {
    label: String,
    container: &'a podman_socket_proxy::backend::DiscoveredContainer,
}

// ── search_images / allow_image ───────────────────────────────────────────────

async fn search_images(args: SearchImagesArgs) -> Result<()> {
    let spinner = make_spinner(format!("Searching Docker Hub for '{}'…", args.query));

    let limit = args.limit.clamp(1, 50);
    let client = Client::builder().build()?;
    let response = client
        .get("https://hub.docker.com/v2/search/repositories/")
        .query(&[("query", args.query.as_str()), ("page_size", &limit.to_string())])
        .send()
        .await?
        .error_for_status()?;
    let body: DockerHubSearchResponse = response.json().await?;

    spinner.finish_and_clear();

    if body.results.is_empty() {
        println!("No results for '{}'.", args.query);
        return Ok(());
    }

    if let Some(mut table) = make_table(&["NAME", "OFFICIAL", "STARS", "DESCRIPTION"]) {
        table.column_mut(3).unwrap().set_constraint(ColumnConstraint::UpperBoundary(Width::Fixed(60)));
        for result in &body.results {
            let official_cell = if result.is_official {
                Cell::new("✓ OFFICIAL").fg(Color::Green)
            } else {
                Cell::new("").fg(Color::DarkGrey)
            };
            let stars_cell = Cell::new(star_rating(result.star_count))
                .fg(Color::Yellow)
                .set_alignment(CellAlignment::Center);
            let desc = result.short_description.as_deref().unwrap_or("").replace('\t', " ");
            let desc = if desc.len() > 60 { format!("{}…", &desc[..57]) } else { desc };

            let name_cell = if result.is_official {
                Cell::new(&result.repo_name).add_attribute(Attribute::Bold)
            } else {
                Cell::new(&result.repo_name)
            };

            table.add_row(vec![
                name_cell,
                official_cell,
                stars_cell,
                Cell::new(desc),
            ]);
        }
        println!("{table}");
    } else {
        // Non-TTY fallback
        println!("NAME\tOFFICIAL\tSTARS\tDESCRIPTION");
        for result in &body.results {
            println!(
                "{}\t{}\t{}\t{}",
                result.repo_name,
                if result.is_official { "yes" } else { "no" },
                result.star_count,
                result.short_description.as_deref().unwrap_or("").replace('\t', " ")
            );
        }
    }

    Ok(())
}

async fn allow_image(args: AllowImageArgs) -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    let policy_path = args.policy.unwrap_or_else(|| resolved.config.policy_path.clone());

    let image = if let Some(img) = args.image {
        img
    } else {
        // Interactive mode: prompt for search query, fetch, picker
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("interactive mode requires a terminal; pass an image name explicitly: psp images allow <image>");
        }

        // Prompt for search
        let query: String = Input::new()
            .with_prompt("Search Docker Hub")
            .interact_text()?;

        let spinner = make_spinner(format!("Searching for '{query}'…"));
        let limit = 15usize;
        let client = Client::builder().build()?;
        let response = client
            .get("https://hub.docker.com/v2/search/repositories/")
            .query(&[("query", query.as_str()), ("page_size", &limit.to_string())])
            .send()
            .await?
            .error_for_status()?;
        let body: DockerHubSearchResponse = response.json().await?;
        spinner.finish_and_clear();

        if body.results.is_empty() {
            bail!("no Docker Hub results for '{query}'");
        }

        // Show results table
        if let Some(mut table) = make_table(&["NAME", "OFFICIAL", "STARS", "DESCRIPTION"]) {
            table.column_mut(3).unwrap().set_constraint(ColumnConstraint::UpperBoundary(Width::Fixed(55)));
            for result in &body.results {
                let official_cell = if result.is_official {
                    Cell::new("✓ OFFICIAL").fg(Color::Green)
                } else {
                    Cell::new("").fg(Color::DarkGrey)
                };
                let stars_cell = Cell::new(star_rating(result.star_count))
                    .fg(Color::Yellow)
                    .set_alignment(CellAlignment::Center);
                let desc = result.short_description.as_deref().unwrap_or("").replace('\t', " ");
                let desc = if desc.len() > 55 { format!("{}…", &desc[..52]) } else { desc };
                let name_cell = if result.is_official {
                    Cell::new(&result.repo_name).add_attribute(Attribute::Bold)
                } else {
                    Cell::new(&result.repo_name)
                };
                table.add_row(vec![name_cell, official_cell, stars_cell, Cell::new(desc)]);
            }
            println!("{table}");
        }

        // Picker
        let items: Vec<String> = body.results.iter().map(|r| {
            format!("{:<40}  {}  {}", r.repo_name,
                if r.is_official { "✓" } else { " " },
                star_rating(r.star_count))
        }).collect();

        let selection = Select::new()
            .with_prompt("Select image to allow")
            .items(&items)
            .default(0)
            .interact()?;

        body.results[selection].repo_name.clone()
    };

    let mut policy = Policy::load(&policy_path)?;
    policy.add_image_allow(&image);
    policy.save(&policy_path)?;
    println!("{} Added {} to allowlist in {}", col_ok("✓"), col_bold(&image), policy_path.display());
    Ok(())
}

// ── smoke_test ────────────────────────────────────────────────────────────────

async fn smoke_test(args: SmokeTestArgs) -> Result<()> {
    let total_start = Instant::now();
    let resolved = Config::resolve_from_env()?;
    let config = &resolved.config;
    let state = build_state(&resolved)?;

    let backend_spinner = make_spinner("Checking backend…");
    state
        .backend_ping()
        .await
        .map_err(|error| anyhow!("backend ping failed: {error:?}"))?;
    backend_spinner.finish_and_clear();

    let temp = TempDir::new()?;
    let socket = temp.path().join("psp-smoke.sock");
    let (shutdown, handle) = spawn_temp_psp(socket.clone(), state).await?;
    let client = new_test_client();

    let t = Instant::now();
    let ping = request_text(&client, &socket, Method::GET, "/_ping", None).await?;
    let elapsed = t.elapsed();
    if ping.0 != StatusCode::OK || ping.1.trim() != "OK" {
        bail!("smoke test ping failed: {} {}", ping.0, ping.1);
    }
    println!("{} daemon ping through PSP ({:.0}ms)", col_ok("PASS"), elapsed.as_secs_f64() * 1000.0);

    let t = Instant::now();
    let denied = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create",
        Some(json!({
            "Image": "postgres:16",
            "HostConfig": {"Privileged": true}
        })),
        Some("smoke-session"),
    )
    .await?;
    let elapsed = t.elapsed();
    if denied.0 != StatusCode::FORBIDDEN {
        bail!("expected denial path to return 403, got {}", denied.0);
    }
    println!("{} policy denial path ({:.0}ms)", col_ok("PASS"), elapsed.as_secs_f64() * 1000.0);

    let image = args
        .image
        .or_else(|| first_allowlisted_image(&config.policy_path).ok().flatten())
        .unwrap_or_else(|| "postgres:16".to_string());

    let pull_spinner = make_spinner(format!("Pulling image {}…", image));
    let t = Instant::now();
    let pull = request_text(
        &client,
        &socket,
        Method::POST,
        &format!("/v1.41/images/create?fromImage={}", percent_encode(&image)),
        Some("smoke-session"),
    )
    .await?;
    let elapsed = t.elapsed();
    pull_spinner.finish_and_clear();
    if !pull.0.is_success() {
        bail!("image pull failed for {}: {}", image, pull.0);
    }
    let _ = elapsed;

    let t = Instant::now();
    let create = request_json(
        &client,
        &socket,
        Method::POST,
        "/v1.41/containers/create?name=psp-smoke",
        Some(json!({"Image": image})),
        Some("smoke-session"),
    )
    .await?;
    if create.0 != StatusCode::CREATED {
        bail!("container create smoke test failed with status {}", create.0);
    }
    let id = create
        .1
        .get("Id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("smoke test create response did not include an Id"))?;

    let remove = request_text(
        &client,
        &socket,
        Method::DELETE,
        &format!("/v1.41/containers/{id}?force=1"),
        Some("smoke-session"),
    )
    .await?;
    if !remove.0.is_success() {
        bail!("container remove smoke test failed with status {}", remove.0);
    }
    let elapsed = t.elapsed();
    println!("{} container lifecycle ({:.0}ms, image: {})", col_ok("PASS"), elapsed.as_secs_f64() * 1000.0, image);

    let _ = shutdown.send(());
    let _ = handle.await;

    let total = total_start.elapsed();
    println!();
    println!("{} Smoke test completed in {:.1}s", col_ok("✓"), total.as_secs_f64());
    Ok(())
}

// ── status ────────────────────────────────────────────────────────────────────

async fn status() -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    let socket = &resolved.config.listen_socket;
    let socket_url = format!("unix://{}", socket.display());

    let (running, detail_lines) = if !socket.exists() {
        (false, vec![
            ("socket".to_string(), format!("{} (not found)", socket.display())),
            ("start with".to_string(), "psp run".to_string()),
        ])
    } else {
        let client = new_test_client();
        match request_text(&client, socket, Method::GET, "/_ping", None).await {
            Ok((status, body)) if status.is_success() && body.trim() == "OK" => {
                (true, vec![
                    ("socket".to_string(), socket_url.clone()),
                    ("export".to_string(), format!("DOCKER_HOST={socket_url}")),
                ])
            }
            Ok((status, _)) => {
                (false, vec![
                    ("socket".to_string(), socket.display().to_string()),
                    ("error".to_string(), format!("unexpected response: {status}")),
                ])
            }
            Err(err) => {
                (false, vec![
                    ("socket".to_string(), socket.display().to_string()),
                    ("error".to_string(), err.to_string()),
                ])
            }
        }
    };

    if io::stdout().is_terminal() {
        let header = if running {
            format!("{}  PSP  RUNNING", col_ok("●"))
        } else {
            format!("{}  PSP  STOPPED", col_err("○"))
        };
        let visible_header_len = visible_len(&header);

        // Calculate box width from visible content
        let content_width = detail_lines
            .iter()
            .map(|(k, v)| k.len() + 2 + v.len())
            .max()
            .unwrap_or(20)
            .max(visible_header_len)
            .max(40);

        let bar: String = "─".repeat(content_width + 4);
        println!("╭{}╮", bar);
        // Header row — pad with spaces based on visible length only
        let header_padding = " ".repeat(content_width.saturating_sub(visible_header_len));
        println!("│  {}{}  │", header, header_padding);
        println!("├{}┤", bar);
        for (key, val) in &detail_lines {
            let line = format!("{:<12}  {}", key, val);
            println!("│  {:<width$}  │", line, width = content_width);
        }
        println!("╰{}╯", bar);
    } else {
        // Non-TTY plain text
        if running {
            println!("PSP is running");
            println!("socket  {socket_url}");
            println!("export  DOCKER_HOST={socket_url}");
        } else {
            println!("PSP is not running");
            for (k, v) in &detail_lines {
                println!("{k}  {v}");
            }
        }
    }

    Ok(())
}

// ── ps ────────────────────────────────────────────────────────────────────────

async fn ps(args: PsArgs) -> Result<()> {
    let spinner = make_spinner("Fetching managed containers…");

    let resolved = Config::resolve_from_env()?;
    let state = build_state(&resolved)?;
    let mut containers = state
        .list_containers(true)
        .await
        .map_err(|error| anyhow!("failed to list containers: {error:?}"))?;

    spinner.finish_and_clear();

    containers.retain(|c| c.metadata.managed);
    if args.running {
        containers.retain(|c| c.state.as_deref() == Some("running"));
    }
    containers.sort_by(|a, b| a.metadata.display_name().cmp(&b.metadata.display_name()));

    if containers.is_empty() {
        println!("No PSP-managed containers.");
        return Ok(());
    }

    if let Some(mut table) = make_table(&["ID", "NAME", "IMAGE", "STATE"]) {
        table.column_mut(2).unwrap().set_constraint(ColumnConstraint::UpperBoundary(Width::Fixed(40)));
        for container in &containers {
            let is_running = container.state.as_deref() == Some("running");
            let state_str = container.state.as_deref()
                .unwrap_or(container.status.as_deref().unwrap_or("unknown"));
            let state_cell = if is_running {
                Cell::new(state_str).fg(Color::Green)
            } else {
                Cell::new(state_str).fg(Color::DarkGrey)
            };
            table.add_row(vec![
                Cell::new(short_id(&container.metadata.id)).fg(Color::DarkGrey),
                Cell::new(container.metadata.display_name()),
                Cell::new(container.metadata.image.as_deref().unwrap_or("")).fg(Color::DarkCyan),
                state_cell,
            ]);
        }
        println!("{table}");
    } else {
        println!("ID\tNAME\tIMAGE\tSTATE");
        for c in &containers {
            let state_str = c.state.as_deref().unwrap_or("unknown");
            println!("{}\t{}\t{}\t{}", short_id(&c.metadata.id), c.metadata.display_name(), c.metadata.image.as_deref().unwrap_or(""), state_str);
        }
    }

    Ok(())
}

// ── env_cmd ───────────────────────────────────────────────────────────────────

fn env_cmd() -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    let socket_url = format!("unix://{}", resolved.config.listen_socket.display());
    let socket_path = resolved.config.listen_socket.display().to_string();

    println!("# PSP integration environment");
    println!("# Run: eval $(psp env)");
    println!("export DOCKER_HOST=\"{socket_url}\"");
    println!("export TESTCONTAINERS_DOCKER_SOCKET_OVERRIDE=\"{socket_path}\"");
    println!("export TESTCONTAINERS_RYUK_DISABLED=\"true\"");

    Ok(())
}

// ── cleanup ───────────────────────────────────────────────────────────────────

async fn cleanup(args: CleanupArgs) -> Result<()> {
    let resolved = Config::resolve_from_env()?;
    let state = build_state(&resolved)?;
    let mut containers = state
        .list_containers(true)
        .await
        .map_err(|error| anyhow!("failed to list containers: {error:?}"))?;

    containers.retain(|c| c.metadata.managed);
    if let Some(ref session_filter) = args.session {
        // We can't filter by session without label data, note this limitation
        let _ = session_filter;
    }

    if containers.is_empty() {
        println!("No PSP-managed containers to clean up.");
        return Ok(());
    }

    println!("PSP-managed containers to remove:");
    for c in &containers {
        let state_str = c.state.as_deref().unwrap_or("unknown");
        println!("  {} {} {} ({})",
            col_warn("~"),
            short_id(&c.metadata.id),
            c.metadata.display_name(),
            col_dim(state_str)
        );
    }
    println!();

    if !args.force {
        if !io::stdin().is_terminal() {
            bail!("non-interactive mode: use --force to remove containers without confirmation");
        }
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("Remove {} PSP-managed container(s)?", containers.len()))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    state.startup_sweep().await
        .map_err(|error| anyhow!("cleanup failed: {error:?}"))?;

    println!("{} Removed {} PSP-managed container(s).", col_ok("✓"), containers.len());
    Ok(())
}

// ── completions ───────────────────────────────────────────────────────────────

fn completions(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    let name = "psp";
    match args.shell {
        CompletionShell::Bash => generate(clap_complete::shells::Bash, &mut cmd, name, &mut io::stdout()),
        CompletionShell::Zsh => generate(clap_complete::shells::Zsh, &mut cmd, name, &mut io::stdout()),
        CompletionShell::Fish => generate(clap_complete::shells::Fish, &mut cmd, name, &mut io::stdout()),
    }
    Ok(())
}

// ── psp_init ──────────────────────────────────────────────────────────────────

async fn psp_init(args: InitArgs) -> Result<()> {
    if args.project {
        // Project-local setup
        let cwd = std::env::current_dir()?;
        let policy_path = cwd.join("policy").join("default-policy.json");
        let config_path = cwd.join(".psp.json");

        println!("{}", col_bold("PSP project setup"));
        println!("{}", col_dim(&"─".repeat(40)));
        println!();

        // Create policy
        if policy_path.exists() && !args.force {
            println!("  {} policy already exists: {}", col_warn("!"), policy_path.display());
            println!("     pass --force to overwrite");
        } else {
            init_policy(InitPolicyArgs {
                output: policy_path.clone(),
                profile: args.profile.clone(),
                force: args.force,
            })?;
            println!("  {} Created policy: {}", col_ok("✓"), policy_path.display());
        }

        // Create .psp.json
        let psp_config = serde_json::json!({
            "policy_path": "policy/default-policy.json"
        });
        if config_path.exists() && !args.force {
            println!("  {} config already exists: {}", col_warn("!"), config_path.display());
        } else {
            fs::write(&config_path, format!("{}\n", serde_json::to_string_pretty(&psp_config)?))?;
            println!("  {} Created config: {}", col_ok("✓"), config_path.display());
        }

        println!();
        println!("{}", col_bold("Next steps:"));
        println!("  1. Start PSP:          psp run");
        println!("  2. Set DOCKER_HOST:    eval $(psp env)");
        println!("  3. Verify:             psp doctor");
    } else {
        // Global setup
        let config_dir = dirs_or_default();
        let policy_path = config_dir.join("policy.json");
        let config_path = config_dir.join("config.json");

        println!("{}", col_bold("PSP global setup"));
        println!("{}", col_dim(&"─".repeat(40)));
        println!();

        if let Some(parent) = policy_path.parent() {
            fs::create_dir_all(parent)?;
        }

        if policy_path.exists() && !args.force {
            println!("  {} policy already exists: {}", col_warn("!"), policy_path.display());
            println!("     pass --force to overwrite");
        } else {
            init_policy(InitPolicyArgs {
                output: policy_path.clone(),
                profile: args.profile.clone(),
                force: args.force,
            })?;
            println!("  {} Created policy: {}", col_ok("✓"), policy_path.display());
        }

        if config_path.exists() && !args.force {
            println!("  {} config already exists: {}", col_warn("!"), config_path.display());
        } else {
            let psp_config = serde_json::json!({
                "policy_path": policy_path.display().to_string()
            });
            fs::write(&config_path, format!("{}\n", serde_json::to_string_pretty(&psp_config)?))?;
            println!("  {} Created config: {}", col_ok("✓"), config_path.display());
        }

        println!();
        println!("{}", col_bold("Next steps:"));
        println!("  1. Start PSP:          psp run");
        println!("  2. Set DOCKER_HOST:    eval $(psp env)");
        println!("  3. Verify:             psp doctor");
        println!();
        println!("  Tip: run 'psp init --profile workspace-postgres' for a Testcontainers starter policy");
    }

    Ok(())
}

fn dirs_or_default() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("psp")
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn build_state(resolved: &ResolvedConfig) -> Result<AppState> {
    let policy = Policy::load(&resolved.config.policy_path)?;
    AppState::new(
        resolved.config.backend.clone(),
        policy,
        resolved.config.advertised_host.clone(),
        resolved.config.keep_on_failure,
        resolved.config.require_session_id,
    )
}

async fn spawn_temp_psp(socket: PathBuf, state: AppState) -> Result<(oneshot::Sender<()>, JoinHandle<()>)> {
    if let Some(parent) = socket.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let _ = tokio::fs::remove_file(&socket).await;
    let listener = UnixListener::bind(&socket)?;
    let app = router(state);
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    Ok((tx, handle))
}

fn new_test_client() -> TestClient<UnixConnector, Full<bytes::Bytes>> {
    TestClient::builder(TokioExecutor::new()).build(UnixConnector)
}

async fn request_json(
    client: &TestClient<UnixConnector, Full<bytes::Bytes>>,
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<Value>,
    session: Option<&str>,
) -> Result<(StatusCode, Value)> {
    let mut builder = HyperRequest::builder()
        .method(method)
        .uri::<hyper::Uri>(hyperlocal::Uri::new(socket, path).into())
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        builder = builder.header(SESSION_HEADER, session);
    }
    let request = builder
        .body(Full::new(bytes::Bytes::from(
            body.map(|v| serde_json::to_vec(&v).unwrap()).unwrap_or_default(),
        )))
        .unwrap();
    let response = client.request(request).await?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let body = if bytes.is_empty() { json!({}) } else { serde_json::from_slice(&bytes)? };
    Ok((status, body))
}

async fn request_text(
    client: &TestClient<UnixConnector, Full<bytes::Bytes>>,
    socket: &Path,
    method: Method,
    path: &str,
    session: Option<&str>,
) -> Result<(StatusCode, String)> {
    let mut builder = HyperRequest::builder()
        .method(method)
        .uri::<hyper::Uri>(hyperlocal::Uri::new(socket, path).into());
    if let Some(session) = session {
        builder = builder.header(SESSION_HEADER, session);
    }
    let request = builder.body(Full::new(bytes::Bytes::new())).unwrap();
    let response = client.request(request).await?;
    let status = response.status();
    let effective_session = response
        .headers()
        .get(EFFECTIVE_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.into_body().collect().await?.to_bytes();
    let body = String::from_utf8(bytes.to_vec())?;
    let _ = effective_session;
    Ok((status, body))
}

fn find_container<'a>(
    containers: &'a [podman_socket_proxy::backend::DiscoveredContainer],
    needle: &str,
) -> Result<&'a podman_socket_proxy::backend::DiscoveredContainer> {
    let normalized = needle.trim_start_matches('/');
    let matches: Vec<_> = containers
        .iter()
        .filter(|container| {
            container.metadata.id == normalized
                || container.metadata.id.starts_with(normalized)
                || container.metadata.names.iter().any(|name| name == normalized)
        })
        .collect();

    match matches.as_slice() {
        [] => bail!("no discovered container matched {}", needle),
        [container] => Ok(*container),
        _ => bail!("multiple discovered containers matched {}; use a more specific id", needle),
    }
}

fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn discovery_policy_path(
    resolved: &ResolvedConfig,
    explicit_policy: Option<PathBuf>,
    project: bool,
) -> Result<PathBuf> {
    if let Some(path) = explicit_policy {
        return Ok(path);
    }

    if project {
        return project_policy_path(resolved);
    }

    global_policy_path(resolved)
}

fn global_policy_path(resolved: &ResolvedConfig) -> Result<PathBuf> {
    if let Some(path) = policy_path_from_config_file(resolved.sources.loaded_global_config.as_deref())? {
        return Ok(path);
    }

    if let Some(config_path) = resolved.sources.global_config_path.as_ref() {
        let parent = config_path.parent().ok_or_else(|| {
            anyhow!("global config path {} has no parent directory", config_path.display())
        })?;
        return Ok(parent.join("policy.json"));
    }

    Ok(resolved.config.policy_path.clone())
}

fn project_policy_path(resolved: &ResolvedConfig) -> Result<PathBuf> {
    if let Some(path) = policy_path_from_config_file(resolved.sources.loaded_project_config.as_deref())? {
        return Ok(path);
    }

    let Some(project_root) = resolved.sources.project_root.as_ref() else {
        bail!("--project requires running inside a git project or worktree")
    };

    Ok(project_root.join("policy/default-policy.json"))
}

fn policy_path_from_config_file(path: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let json: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;

    let policy = json
        .get("policy_path")
        .or_else(|| json.get("policy_file"))
        .and_then(Value::as_str);
    let Some(policy) = policy else {
        return Ok(None);
    };

    let policy_path = PathBuf::from(policy);
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    Ok(Some(if policy_path.is_absolute() {
        policy_path
    } else {
        base_dir.join(policy_path)
    }))
}

fn first_allowlisted_image(path: &Path) -> Result<Option<String>> {
    let policy = Policy::load(path)?;
    Ok(policy.images.allowlist.first().cloned())
}

fn percent_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

// ── data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DockerHubSearchResponse {
    results: Vec<DockerHubSearchResult>,
}

#[derive(Debug, Deserialize)]
struct DockerHubSearchResult {
    repo_name: String,
    is_official: bool,
    star_count: u64,
    short_description: Option<String>,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use podman_socket_proxy::{
        backend::DiscoveredContainer,
        policy::{ContainerAccessPolicy, ContainerMetadata},
    };

    fn test_policy() -> Policy {
        let mut policy = Policy {
            version: "v1".into(),
            bind_mounts: Default::default(),
            images: Default::default(),
            containers: ContainerAccessPolicy {
                allowlist: vec!["shared-db".into()],
                denylist: vec!["prod-db".into()],
                ..Default::default()
            },
        };
        policy.precompute();
        policy
    }

    fn discovered(name: &str, id: &str, access_image: &str) -> DiscoveredContainer {
        DiscoveredContainer {
            metadata: ContainerMetadata {
                id: id.into(),
                names: vec![name.into()],
                image: Some(access_image.into()),
                managed: false,
            },
            state: Some("running".into()),
            status: Some("Up 10s".into()),
        }
    }

    #[test]
    fn interactive_allow_candidates_skip_already_allowed_and_managed() {
        let policy = test_policy();
        let mut managed = discovered("psp-managed", "cid-managed", "postgres:16");
        managed.metadata.managed = true;
        let containers = vec![
            discovered("shared-db", "cid-1", "postgres:16"),
            discovered("other-db", "cid-2", "postgres:16"),
            managed,
        ];

        let labels: Vec<_> = interactive_candidates(&containers, &policy, true)
            .into_iter()
            .map(|candidate| candidate.label)
            .collect();

        assert_eq!(labels.len(), 1);
        assert!(labels[0].contains("other-db"));
    }

    #[test]
    fn discovery_policy_defaults_to_global_scope() {
        let resolved = ResolvedConfig {
            config: Config {
                listen_socket: "/tmp/psp.sock".into(),
                backend: podman_socket_proxy::BackendConfig::Unix("/tmp/podman.sock".into()),
                policy_path: "policy/default-policy.json".into(),
                advertised_host: "127.0.0.1".into(),
                keep_on_failure: false,
                require_session_id: false,
            },
            sources: podman_socket_proxy::ConfigSources {
                global_config_path: Some("/home/test/.config/psp/config.json".into()),
                loaded_global_config: None,
                project_root: Some("/repo".into()),
                project_config_path: Some("/repo/.psp.json".into()),
                loaded_project_config: None,
            },
        };

        let path = discovery_policy_path(&resolved, None, false).unwrap();
        assert_eq!(path, PathBuf::from("/home/test/.config/psp/policy.json"));
    }

    #[test]
    fn discovery_policy_uses_project_scope_when_requested() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("repo");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(
            project_root.join(".psp.json"),
            r#"{ "policy_path": ".psp/local-policy.json" }"#,
        )
        .unwrap();

        let resolved = ResolvedConfig {
            config: Config {
                listen_socket: "/tmp/psp.sock".into(),
                backend: podman_socket_proxy::BackendConfig::Unix("/tmp/podman.sock".into()),
                policy_path: "policy/default-policy.json".into(),
                advertised_host: "127.0.0.1".into(),
                keep_on_failure: false,
                require_session_id: false,
            },
            sources: podman_socket_proxy::ConfigSources {
                global_config_path: None,
                loaded_global_config: None,
                project_root: Some(project_root.clone()),
                project_config_path: Some(project_root.join(".psp.json")),
                loaded_project_config: Some(project_root.join(".psp.json")),
            },
        };

        let path = discovery_policy_path(&resolved, None, true).unwrap();
        assert_eq!(path, project_root.join(".psp/local-policy.json"));
    }

    #[test]
    fn discovery_policy_requires_project_root_for_project_scope() {
        let resolved = ResolvedConfig {
            config: Config {
                listen_socket: "/tmp/psp.sock".into(),
                backend: podman_socket_proxy::BackendConfig::Unix("/tmp/podman.sock".into()),
                policy_path: "policy/default-policy.json".into(),
                advertised_host: "127.0.0.1".into(),
                keep_on_failure: false,
                require_session_id: false,
            },
            sources: podman_socket_proxy::ConfigSources::default(),
        };

        let error = discovery_policy_path(&resolved, None, true).unwrap_err();
        assert!(error.to_string().contains("--project"));
    }

    #[test]
    fn interactive_deny_candidates_skip_already_denied() {
        let policy = test_policy();
        let containers = vec![
            discovered("prod-db", "cid-1", "postgres:16"),
            discovered("shared-db", "cid-2", "postgres:16"),
            discovered("other-db", "cid-3", "postgres:16"),
        ];

        let labels: Vec<_> = interactive_candidates(&containers, &policy, false)
            .into_iter()
            .map(|candidate| candidate.label)
            .collect();

        assert_eq!(labels.len(), 2);
        assert!(labels.iter().any(|label| label.contains("shared-db")));
        assert!(labels.iter().any(|label| label.contains("other-db")));
        assert!(!labels.iter().any(|label| label.contains("prod-db")));
    }
}
