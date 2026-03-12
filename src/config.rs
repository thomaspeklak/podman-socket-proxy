use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use url::Url;

const DEFAULT_LISTEN_SOCKET: &str = "/tmp/psp.sock";
const DEFAULT_ADVERTISED_HOST: &str = "127.0.0.1";
const GLOBAL_CONFIG_SUFFIX: &str = "psp/config.json";
const PROJECT_CONFIG_FILE: &str = ".psp.json";
pub const COMPATIBILITY_PROFILE: &str = "testcontainers-v1";

#[derive(Clone, Debug, Serialize)]
pub struct Config {
    pub listen_socket: PathBuf,
    #[serde(serialize_with = "serialize_backend")]
    pub backend: BackendConfig,
    pub policy_path: PathBuf,
    pub advertised_host: String,
    pub keep_on_failure: bool,
    pub require_session_id: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self::resolve_from_env()?.config)
    }

    pub fn resolve_from_env() -> Result<ResolvedConfig> {
        let cwd = std::env::current_dir().context("failed to determine current working directory")?;
        let env = EnvConfig::from_process();
        Self::resolve(&cwd, &env)
    }

    fn resolve(cwd: &Path, env: &EnvConfig) -> Result<ResolvedConfig> {
        let mut raw = RawConfig::defaults(env);
        let mut sources = ConfigSources::default();

        if let Some(path) = global_config_path(env) {
            sources.global_config_path = Some(path.clone());
            if path.exists() {
                raw.apply_file(ConfigFile::load(&path)?, &path);
                sources.loaded_global_config = Some(path);
            }
        }

        if let Some(project_root) = discover_project_root(cwd)? {
            sources.project_root = Some(project_root.clone());
            let path = project_root.join(PROJECT_CONFIG_FILE);
            sources.project_config_path = Some(path.clone());
            if path.exists() {
                raw.apply_file(ConfigFile::load(&path)?, &path);
                sources.loaded_project_config = Some(path);
            }
        }

        raw.apply_env(env);

        Ok(ResolvedConfig {
            config: raw.build()?,
            sources,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedConfig {
    pub config: Config,
    pub sources: ConfigSources,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ConfigSources {
    pub global_config_path: Option<PathBuf>,
    pub loaded_global_config: Option<PathBuf>,
    pub project_root: Option<PathBuf>,
    pub project_config_path: Option<PathBuf>,
    pub loaded_project_config: Option<PathBuf>,
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

    pub fn display_string(&self) -> String {
        match self {
            Self::Http(url) => url.as_str().to_string(),
            Self::Unix(path) => format!("unix://{}", path.display()),
        }
    }
}

fn serialize_backend<S>(backend: &BackendConfig, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&backend.display_string())
}

#[derive(Clone, Debug)]
struct RawConfig {
    listen_socket: PathBuf,
    backend: String,
    policy_path: PathBuf,
    advertised_host: String,
    keep_on_failure: bool,
    require_session_id: bool,
}

impl RawConfig {
    fn defaults(env: &EnvConfig) -> Self {
        Self {
            listen_socket: PathBuf::from(DEFAULT_LISTEN_SOCKET),
            backend: default_backend_raw(env),
            policy_path: PathBuf::from("policy/default-policy.json"),
            advertised_host: DEFAULT_ADVERTISED_HOST.to_string(),
            keep_on_failure: false,
            require_session_id: false,
        }
    }

    fn apply_file(&mut self, config: ConfigFile, source: &Path) {
        let base_dir = source.parent().unwrap_or_else(|| Path::new("."));

        if let Some(listen_socket) = config.listen_socket {
            self.listen_socket = resolve_path(base_dir, &listen_socket);
        }

        if let Some(backend) = config.backend {
            self.backend = resolve_backend(base_dir, &backend);
        }

        if let Some(policy_path) = config.policy_path {
            self.policy_path = resolve_path(base_dir, &policy_path);
        }

        if let Some(advertised_host) = config.advertised_host {
            self.advertised_host = advertised_host;
        }

        if let Some(keep_on_failure) = config.keep_on_failure {
            self.keep_on_failure = keep_on_failure;
        }

        if let Some(require_session_id) = config.require_session_id {
            self.require_session_id = require_session_id;
        }
    }

    fn apply_env(&mut self, env: &EnvConfig) {
        if let Some(listen_socket) = &env.psp_listen_socket {
            self.listen_socket = listen_socket.clone();
        }

        if let Some(backend) = &env.psp_backend {
            self.backend = backend.clone();
        }

        if let Some(policy_path) = &env.psp_policy_file {
            self.policy_path = policy_path.clone();
        }

        if let Some(advertised_host) = &env.psp_advertised_host {
            self.advertised_host = advertised_host.clone();
        }

        if let Some(keep_on_failure) = env.psp_keep_on_failure {
            self.keep_on_failure = keep_on_failure;
        }

        if let Some(require_session_id) = env.psp_require_session_id {
            self.require_session_id = require_session_id;
        }
    }

    fn build(self) -> Result<Config> {
        Ok(Config {
            listen_socket: self.listen_socket,
            backend: BackendConfig::parse(&self.backend)?,
            policy_path: self.policy_path,
            advertised_host: self.advertised_host,
            keep_on_failure: self.keep_on_failure,
            require_session_id: self.require_session_id,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    listen_socket: Option<PathBuf>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default, alias = "policy_file")]
    policy_path: Option<PathBuf>,
    #[serde(default)]
    advertised_host: Option<String>,
    #[serde(default)]
    keep_on_failure: Option<bool>,
    #[serde(default)]
    require_session_id: Option<bool>,
}

impl ConfigFile {
    fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse config file {}", path.display()))
    }
}

#[derive(Clone, Debug, Default)]
struct EnvConfig {
    psp_listen_socket: Option<PathBuf>,
    psp_backend: Option<String>,
    psp_policy_file: Option<PathBuf>,
    psp_advertised_host: Option<String>,
    psp_keep_on_failure: Option<bool>,
    psp_require_session_id: Option<bool>,
    xdg_runtime_dir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl EnvConfig {
    fn from_process() -> Self {
        Self {
            psp_listen_socket: std::env::var_os("PSP_LISTEN_SOCKET").map(PathBuf::from),
            psp_backend: std::env::var("PSP_BACKEND").ok(),
            psp_policy_file: std::env::var_os("PSP_POLICY_FILE").map(PathBuf::from),
            psp_advertised_host: std::env::var("PSP_ADVERTISED_HOST").ok(),
            psp_keep_on_failure: std::env::var("PSP_KEEP_ON_FAILURE")
                .ok()
                .as_deref()
                .and_then(parse_bool),
            psp_require_session_id: std::env::var("PSP_REQUIRE_SESSION_ID")
                .ok()
                .as_deref()
                .and_then(parse_bool),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

fn default_backend_raw(env: &EnvConfig) -> String {
    let runtime_dir = env
        .xdg_runtime_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", nix::unistd::getuid())));
    format!("unix://{}/podman/podman.sock", runtime_dir.display())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" | "TRUE" | "yes" | "YES" => Some(true),
        "0" | "false" | "FALSE" | "no" | "NO" => Some(false),
        _ => None,
    }
}

fn global_config_path(env: &EnvConfig) -> Option<PathBuf> {
    env.xdg_config_home
        .as_ref()
        .map(|path| path.join(GLOBAL_CONFIG_SUFFIX))
        .or_else(|| {
            env.home
                .as_ref()
                .map(|path| path.join(".config").join(GLOBAL_CONFIG_SUFFIX))
        })
}

fn discover_project_root(start: &Path) -> Result<Option<PathBuf>> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent().unwrap_or(start).to_path_buf()
    };

    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() {
            return Ok(Some(current));
        }

        if dot_git.is_file() {
            return Ok(Some(resolve_root_from_git_file(&current, &dot_git)?));
        }

        let Some(parent) = current.parent() else {
            return Ok(None);
        };
        current = parent.to_path_buf();
    }
}

fn resolve_root_from_git_file(worktree_root: &Path, dot_git: &Path) -> Result<PathBuf> {
    let git_dir = parse_git_dir(dot_git)?;

    if git_dir.file_name() == Some(OsStr::new(".git")) {
        return git_dir
            .parent()
            .map(Path::to_path_buf)
            .context("gitdir file pointed at .git without a repository root");
    }

    if git_dir.parent().and_then(Path::file_name) == Some(OsStr::new("worktrees")) {
        return git_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .context("git worktree path did not resolve to a shared repository root");
    }

    Ok(worktree_root.to_path_buf())
}

fn parse_git_dir(dot_git: &Path) -> Result<PathBuf> {
    let content = fs::read_to_string(dot_git)
        .with_context(|| format!("failed to read {}", dot_git.display()))?;
    let raw = content
        .strip_prefix("gitdir:")
        .map(str::trim)
        .ok_or_else(|| anyhow!("invalid .git file format at {}", dot_git.display()))?;

    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        dot_git
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    };

    fs::canonicalize(&resolved)
        .with_context(|| format!("failed to resolve gitdir {}", resolved.display()))
}

fn resolve_path(base_dir: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        base_dir.join(value)
    }
}

fn resolve_backend(base_dir: &Path, value: &str) -> String {
    if let Some(path) = value.strip_prefix("unix://") {
        let path = Path::new(path);
        let resolved = resolve_path(base_dir, path);
        format!("unix://{}", resolved.display())
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn merges_global_then_project_then_env_config() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let repo_subdir = repo.join("src/bin");

        fs::create_dir_all(home.join(".config/psp")).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&repo_subdir).unwrap();

        fs::write(
            home.join(".config/psp/config.json"),
            r#"{
                "backend": "http://127.0.0.1:8080",
                "listen_socket": "global.sock",
                "keep_on_failure": true,
                "advertised_host": "global.test",
                "require_session_id": true
            }"#,
        )
        .unwrap();

        fs::write(
            repo.join(".psp.json"),
            r#"{
                "policy_path": "policy/project.json",
                "advertised_host": "project.test",
                "keep_on_failure": false
            }"#,
        )
        .unwrap();

        let env = EnvConfig {
            home: Some(home.clone()),
            psp_listen_socket: Some(PathBuf::from("/tmp/env.sock")),
            psp_require_session_id: Some(false),
            ..Default::default()
        };

        let resolved = Config::resolve(&repo_subdir, &env).unwrap();
        let config = resolved.config;

        assert_eq!(config.listen_socket, PathBuf::from("/tmp/env.sock"));
        assert!(matches!(config.backend, BackendConfig::Http(_)));
        assert_eq!(config.policy_path, repo.join("policy/project.json"));
        assert_eq!(config.advertised_host, "project.test");
        assert!(!config.keep_on_failure);
        assert!(!config.require_session_id);
        assert_eq!(
            resolved.sources.loaded_global_config,
            Some(home.join(".config/psp/config.json"))
        );
        assert_eq!(resolved.sources.project_root, Some(repo));
    }

    #[test]
    fn resolves_shared_repo_root_for_worktrees() {
        let temp = tempdir().unwrap();
        let main_repo = temp.path().join("main-repo");
        let worktree = temp.path().join("worktrees/feature");
        let worktree_subdir = worktree.join("nested");
        let shared_gitdir = main_repo.join(".git/worktrees/feature");

        fs::create_dir_all(main_repo.join(".git")).unwrap();
        fs::create_dir_all(&shared_gitdir).unwrap();
        fs::create_dir_all(&worktree_subdir).unwrap();

        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", shared_gitdir.display()),
        )
        .unwrap();
        fs::write(
            main_repo.join(".psp.json"),
            r#"{
                "policy_path": "policy/shared.json",
                "advertised_host": "shared.test"
            }"#,
        )
        .unwrap();

        let resolved = Config::resolve(&worktree_subdir, &EnvConfig::default()).unwrap();
        let config = resolved.config;

        assert_eq!(config.policy_path, main_repo.join("policy/shared.json"));
        assert_eq!(config.advertised_host, "shared.test");
        assert_eq!(resolved.sources.project_root, Some(main_repo));
    }

    #[test]
    fn falls_back_to_defaults_without_config_files() {
        let temp = tempdir().unwrap();
        let cwd = temp.path();
        let env = EnvConfig {
            xdg_runtime_dir: Some(PathBuf::from("/run/user/4242")),
            ..Default::default()
        };

        let resolved = Config::resolve(cwd, &env).unwrap();
        let config = resolved.config;

        assert_eq!(config.listen_socket, PathBuf::from(DEFAULT_LISTEN_SOCKET));
        assert_eq!(config.policy_path, PathBuf::from("policy/default-policy.json"));
        assert_eq!(config.advertised_host, DEFAULT_ADVERTISED_HOST);
        assert!(!config.keep_on_failure);
        assert!(!config.require_session_id);
        match config.backend {
            BackendConfig::Unix(path) => {
                assert_eq!(path, PathBuf::from("/run/user/4242/podman/podman.sock"));
            }
            BackendConfig::Http(url) => panic!("expected unix backend, got {url}"),
        }
    }

    #[test]
    fn parses_false_env_values() {
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }
}
