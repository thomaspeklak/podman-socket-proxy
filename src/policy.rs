use std::{fs, path::Path};

use anyhow::{Result, bail};
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::form_urlencoded;

pub const POLICY_SCHEMA_VERSION: &str = "v1";
pub const RULE_PARSE_ERROR: &str = "PSP-POL-000";
pub const RULE_PRIVILEGED: &str = "PSP-POL-001";
pub const RULE_HOST_NAMESPACE: &str = "PSP-POL-002";
pub const RULE_BIND_MOUNT: &str = "PSP-POL-003";
pub const RULE_DEVICE_MOUNT: &str = "PSP-POL-004";
pub const RULE_CAP_ADD: &str = "PSP-POL-005";
pub const RULE_IMAGE_DENYLIST: &str = "PSP-POL-006";
pub const RULE_IMAGE_ALLOWLIST: &str = "PSP-POL-007";
pub const RULE_CONTAINER_DENYLIST: &str = "PSP-POL-008";
pub const RULE_CONTAINER_ALLOWLIST: &str = "PSP-POL-009";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
    pub version: String,
    #[serde(default)]
    pub bind_mounts: BindMountPolicy,
    #[serde(default)]
    pub images: ImagePolicy,
    #[serde(default)]
    pub containers: ContainerAccessPolicy,
}

impl Policy {
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let mut policy: Policy = serde_json::from_str(&content)?;
        policy.validate()?;
        policy.precompute();
        Ok(policy)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, format!("{json}\n"))?;
        Ok(())
    }

    /// Pre-compute normalized forms for bind mount, image, and container entries.
    pub fn precompute(&mut self) {
        self.bind_mounts.normalized_allowlist = self
            .bind_mounts
            .allowlist
            .iter()
            .map(|p| normalize_bind_path(p))
            .collect();
        self.images.normalized_allowlist = self
            .images
            .allowlist
            .iter()
            .map(|i| normalize_image_ref(i))
            .collect();
        self.images.normalized_denylist = self
            .images
            .denylist
            .iter()
            .map(|i| normalize_image_ref(i))
            .collect();
        self.containers.normalized_allowlist = self
            .containers
            .allowlist
            .iter()
            .map(|entry| normalize_container_ref(entry))
            .collect();
        self.containers.normalized_denylist = self
            .containers
            .denylist
            .iter()
            .map(|entry| normalize_container_ref(entry))
            .collect();
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != POLICY_SCHEMA_VERSION {
            bail!(
                "unsupported policy version: {} (expected {})",
                self.version,
                POLICY_SCHEMA_VERSION
            );
        }

        for path in &self.bind_mounts.allowlist {
            if !path.starts_with('/') {
                bail!("bind mount allowlist entries must be absolute paths: {path}");
            }
        }

        Ok(())
    }

    /// Evaluate a request against policy. `normalized` must be a pre-normalized path.
    pub fn evaluate_request(
        &self,
        method: &Method,
        normalized: &str,
        query: Option<&str>,
        body: &[u8],
    ) -> Result<(), Denial> {
        match (method.as_str(), normalized) {
            ("POST", "/containers/create") => self.evaluate_container_create(body),
            ("POST", "/images/create") => self.evaluate_image_pull(query),
            _ => Ok(()),
        }
    }

    fn evaluate_container_create(&self, body: &[u8]) -> Result<(), Denial> {
        let request: ContainerCreateRequest = serde_json::from_slice(body).map_err(|error| {
            Denial::new(
                RULE_PARSE_ERROR,
                format!("invalid container create payload: {error}"),
            )
        })?;
        self.evaluate_container_create_inner(&request)
    }

    fn evaluate_container_create_inner(
        &self,
        request: &ContainerCreateRequest,
    ) -> Result<(), Denial> {
        if request.host_config.privileged.unwrap_or(false) {
            return Err(Denial::new(
                RULE_PRIVILEGED,
                "privileged containers are denied by default",
            ));
        }

        for namespace in [
            request.host_config.network_mode.as_deref(),
            request.host_config.pid_mode.as_deref(),
            request.host_config.ipc_mode.as_deref(),
            request.host_config.uts_mode.as_deref(),
            request.host_config.userns_mode.as_deref(),
            request.host_config.cgroupns_mode.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if is_host_namespace(namespace) {
                return Err(Denial::new(
                    RULE_HOST_NAMESPACE,
                    format!("host namespace mode is denied: {namespace}"),
                ));
            }
        }

        if !request.host_config.devices.is_empty() {
            return Err(Denial::new(
                RULE_DEVICE_MOUNT,
                "device mounts are denied by default",
            ));
        }

        if !request.host_config.cap_add.is_empty() {
            return Err(Denial::new(
                RULE_CAP_ADD,
                "capability additions are denied by default",
            ));
        }

        for bind in &request.host_config.binds {
            let source = bind_source(bind);
            if !self.bind_mounts.is_allowed(&source) {
                return Err(Denial::new(
                    RULE_BIND_MOUNT,
                    format!("bind mount source is not allowlisted: {source}"),
                ));
            }
        }

        for mount in &request.host_config.mounts {
            if mount.kind.eq_ignore_ascii_case("bind") {
                let source = mount.source.clone().unwrap_or_default();
                if !self.bind_mounts.is_allowed(&source) {
                    return Err(Denial::new(
                        RULE_BIND_MOUNT,
                        format!("bind mount source is not allowlisted: {source}"),
                    ));
                }
            }
        }

        self.evaluate_image_reference(&request.image)
    }

    fn evaluate_image_pull(&self, query: Option<&str>) -> Result<(), Denial> {
        let from_image = query
            .into_iter()
            .flat_map(|q| form_urlencoded::parse(q.as_bytes()))
            .find(|(key, _)| key == "fromImage")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();

        if from_image.is_empty() {
            return Ok(());
        }

        self.evaluate_image_reference(&from_image)
    }

    pub fn evaluate_image_reference(&self, image: &str) -> Result<(), Denial> {
        let normalized = normalize_image_ref(image);

        if self.images.denylist.iter().any(|entry| image == entry)
            || self
                .images
                .normalized_denylist
                .iter()
                .any(|n| *n == normalized)
        {
            return Err(Denial::new(
                RULE_IMAGE_DENYLIST,
                format!("image is explicitly denied by policy: {image}"),
            ));
        }

        if !self.images.allowlist.is_empty()
            && !self.images.allowlist.iter().any(|entry| image == entry)
            && !self
                .images
                .normalized_allowlist
                .iter()
                .any(|n| *n == normalized)
        {
            return Err(Denial::new(
                RULE_IMAGE_ALLOWLIST,
                format!("image is not present in the allowlist: {image}"),
            ));
        }

        Ok(())
    }

    pub fn evaluate_container_access(&self, container: &ContainerMetadata) -> Result<(), Denial> {
        if container.managed {
            return Ok(());
        }

        if self.containers.matches_deny(container) {
            return Err(Denial::new(
                RULE_CONTAINER_DENYLIST,
                format!(
                    "container is explicitly denied by policy: {}",
                    container.display_name()
                ),
            ));
        }

        if !self.containers.matches_allow(container) {
            return Err(Denial::new(
                RULE_CONTAINER_ALLOWLIST,
                format!(
                    "container is not present in the allowlist: {}",
                    container.display_name()
                ),
            ));
        }

        Ok(())
    }

    pub fn add_container_allow(&mut self, entry: &str) {
        upsert_sorted(&mut self.containers.allowlist, entry);
        remove_matching(&mut self.containers.denylist, entry);
        self.precompute();
    }

    pub fn add_container_deny(&mut self, entry: &str) {
        upsert_sorted(&mut self.containers.denylist, entry);
        remove_matching(&mut self.containers.allowlist, entry);
        self.precompute();
    }

    pub fn add_image_allow(&mut self, image: &str) {
        upsert_sorted(&mut self.images.allowlist, image);
        remove_matching(&mut self.images.denylist, image);
        self.precompute();
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BindMountPolicy {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(skip)]
    pub normalized_allowlist: Vec<String>,
}

impl BindMountPolicy {
    fn is_allowed(&self, source: &str) -> bool {
        if source.is_empty() {
            return false;
        }
        let canonicalized = normalize_bind_path(source);
        self.normalized_allowlist.iter().any(|norm_prefix| {
            canonicalized == *norm_prefix
                || (canonicalized.starts_with(norm_prefix.as_str())
                    && canonicalized.as_bytes().get(norm_prefix.len()) == Some(&b'/'))
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImagePolicy {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub denylist: Vec<String>,
    #[serde(skip)]
    pub normalized_allowlist: Vec<String>,
    #[serde(skip)]
    pub normalized_denylist: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContainerAccessPolicy {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub denylist: Vec<String>,
    #[serde(skip)]
    pub normalized_allowlist: Vec<String>,
    #[serde(skip)]
    pub normalized_denylist: Vec<String>,
}

impl ContainerAccessPolicy {
    pub fn matches_allow(&self, container: &ContainerMetadata) -> bool {
        self.matches_any(&self.allowlist, &self.normalized_allowlist, container)
    }

    pub fn matches_deny(&self, container: &ContainerMetadata) -> bool {
        self.matches_any(&self.denylist, &self.normalized_denylist, container)
    }

    fn matches_any(
        &self,
        raw_entries: &[String],
        _normalized_entries: &[String],
        container: &ContainerMetadata,
    ) -> bool {
        let candidates = container.match_candidates();
        let normalized_candidates: Vec<String> = candidates
            .iter()
            .map(|candidate| normalize_container_ref(candidate))
            .collect();
        raw_entries.iter().any(|entry| {
            let normalized_entry = normalize_container_ref(entry);
            candidates.contains(&entry.as_str())
                || normalized_candidates.iter().any(|candidate| candidate == &normalized_entry)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    pub rule_id: &'static str,
    pub reason: String,
}

impl Denial {
    pub fn new(rule_id: &'static str, reason: impl Into<String>) -> Self {
        Self {
            rule_id,
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerMetadata {
    pub id: String,
    pub names: Vec<String>,
    pub image: Option<String>,
    pub managed: bool,
}

impl ContainerMetadata {
    pub fn display_name(&self) -> String {
        self.names
            .first()
            .cloned()
            .unwrap_or_else(|| self.id.clone())
    }

    fn match_candidates(&self) -> Vec<&str> {
        let mut candidates = Vec::with_capacity(self.names.len() + 1);
        candidates.push(self.id.as_str());
        for name in &self.names {
            candidates.push(name.as_str());
        }
        candidates
    }
}

/// Lexically normalize a path: resolve `.` and `..` without touching the filesystem.
fn normalize_bind_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in std::path::Path::new(path).components() {
        match component {
            std::path::Component::RootDir => parts.clear(),
            std::path::Component::Normal(s) => {
                if let Some(s) = s.to_str() {
                    parts.push(s);
                }
            }
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    format!("/{}", parts.join("/"))
}

/// Normalize a Docker image reference to its fully-qualified form.
///
/// - `postgres:16` → `docker.io/library/postgres:16`
/// - `myuser/myimage` → `docker.io/myuser/myimage:latest`
/// - `ghcr.io/org/image:v1` → unchanged
fn normalize_image_ref(image: &str) -> String {
    let (name, suffix) = if let Some(idx) = image.find('@') {
        (&image[..idx], &image[idx..])
    } else if let Some(idx) = image.rfind(':') {
        if image[idx + 1..].contains('/') {
            (image, ":latest")
        } else {
            (&image[..idx], &image[idx..])
        }
    } else {
        (image, ":latest")
    };

    let parts: Vec<&str> = name.split('/').collect();
    let full_name = match parts.len() {
        1 => format!("docker.io/library/{name}"),
        2 if !parts[0].contains('.') && !parts[0].contains(':') => {
            format!("docker.io/{name}")
        }
        _ => name.to_string(),
    };

    format!("{full_name}{suffix}")
}

fn normalize_container_ref(entry: &str) -> String {
    entry.trim().trim_start_matches('/').to_string()
}

#[derive(Debug, Deserialize)]
struct ContainerCreateRequest {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "HostConfig", default)]
    host_config: HostConfig,
}

#[derive(Debug, Default, Deserialize)]
struct HostConfig {
    #[serde(rename = "Privileged")]
    privileged: Option<bool>,
    #[serde(rename = "NetworkMode")]
    network_mode: Option<String>,
    #[serde(rename = "PidMode")]
    pid_mode: Option<String>,
    #[serde(rename = "IpcMode")]
    ipc_mode: Option<String>,
    #[serde(rename = "UTSMode")]
    uts_mode: Option<String>,
    #[serde(rename = "UsernsMode")]
    userns_mode: Option<String>,
    #[serde(rename = "CgroupnsMode")]
    cgroupns_mode: Option<String>,
    #[serde(rename = "Binds", default)]
    binds: Vec<String>,
    #[serde(rename = "Mounts", default)]
    mounts: Vec<Mount>,
    #[serde(rename = "Devices", default)]
    devices: Vec<Value>,
    #[serde(rename = "CapAdd", default)]
    cap_add: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Mount {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "Source")]
    source: Option<String>,
}

fn is_host_namespace(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    normalized == "host" || normalized.starts_with("container:")
}

fn bind_source(bind: &str) -> String {
    bind.split(':').next().unwrap_or_default().to_string()
}

fn upsert_sorted(entries: &mut Vec<String>, value: &str) {
    if !entries.iter().any(|entry| entry == value) {
        entries.push(value.to_string());
        entries.sort();
    }
}

fn remove_matching(entries: &mut Vec<String>, value: &str) {
    let normalized = normalize_container_ref(value);
    entries.retain(|entry| {
        if entry == value {
            return false;
        }
        if normalize_container_ref(entry) == normalized {
            return false;
        }
        if normalize_image_ref(entry) == normalize_image_ref(value) && entry.contains(':') {
            return false;
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn policy() -> Policy {
        let mut p = Policy {
            version: POLICY_SCHEMA_VERSION.to_string(),
            bind_mounts: BindMountPolicy {
                allowlist: vec!["/workspace".into()],
                ..Default::default()
            },
            images: ImagePolicy {
                allowlist: vec!["postgres:16".into(), "redis:7".into()],
                denylist: vec!["alpine:latest".into()],
                ..Default::default()
            },
            containers: ContainerAccessPolicy {
                allowlist: vec!["shared-db".into(), "cid-allow".into()],
                denylist: vec!["blocked-db".into(), "cid-deny".into()],
                ..Default::default()
            },
        };
        p.precompute();
        p
    }

    #[test]
    fn validates_policy_schema_version() {
        let mut policy = policy();
        policy.version = "v0".into();
        assert!(policy.validate().is_err());
    }

    #[test]
    fn allows_safe_container_create() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": {
                "Binds": ["/workspace/tmp:/tmp"]
            }
        }))
        .unwrap();
        assert!(
            policy()
                .evaluate_request(&Method::POST, "/containers/create", None, &body)
                .is_ok()
        );
    }

    #[test]
    fn denies_privileged_mode() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "Privileged": true }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_PRIVILEGED);
    }

    #[test]
    fn denies_host_namespace_mode() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "NetworkMode": "host" }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_HOST_NAMESPACE);
    }

    #[test]
    fn denies_unallowlisted_bind_mount() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "Binds": ["/home/tom/.ssh:/root/.ssh:ro"] }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_BIND_MOUNT);
    }

    #[test]
    fn denies_device_mounts() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "Devices": [{"PathOnHost": "/dev/kvm"}] }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_DEVICE_MOUNT);
    }

    #[test]
    fn denies_capability_adds() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "CapAdd": ["NET_ADMIN"] }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_CAP_ADD);
    }

    #[test]
    fn denies_image_denylist_entries() {
        let denial = policy()
            .evaluate_request(
                &Method::POST,
                "/images/create",
                Some("fromImage=alpine%3Alatest"),
                &[],
            )
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_IMAGE_DENYLIST);
    }

    #[test]
    fn denies_images_outside_allowlist() {
        let body = serde_json::to_vec(&json!({"Image": "nginx:latest"})).unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_IMAGE_ALLOWLIST);
    }

    #[test]
    fn denies_path_traversal_in_bind_mount() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "Binds": ["/workspace/../etc/shadow:/mnt/shadow:ro"] }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_BIND_MOUNT);
    }

    #[test]
    fn denies_path_traversal_in_mount() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": {
                "Mounts": [{"Type": "bind", "Source": "/workspace/../../etc/passwd", "Target": "/mnt"}]
            }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_BIND_MOUNT);
    }

    #[test]
    fn allows_fully_qualified_image_matching_short_allowlist() {
        let body = serde_json::to_vec(&json!({"Image": "docker.io/library/postgres:16"}))
            .unwrap();
        assert!(
            policy()
                .evaluate_request(&Method::POST, "/containers/create", None, &body)
                .is_ok()
        );
    }

    #[test]
    fn denies_fully_qualified_denylist_match() {
        let denial = policy()
            .evaluate_request(
                &Method::POST,
                "/images/create",
                Some("fromImage=docker.io%2Flibrary%2Falpine%3Alatest"),
                &[],
            )
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_IMAGE_DENYLIST);
    }

    #[test]
    fn denies_host_userns_mode() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "UsernsMode": "host" }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_HOST_NAMESPACE);
    }

    #[test]
    fn denies_host_cgroupns_mode() {
        let body = serde_json::to_vec(&json!({
            "Image": "postgres:16",
            "HostConfig": { "CgroupnsMode": "host" }
        }))
        .unwrap();
        let denial = policy()
            .evaluate_request(&Method::POST, "/containers/create", None, &body)
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_HOST_NAMESPACE);
    }

    #[test]
    fn denies_non_managed_container_without_allowlist_entry() {
        let denial = policy()
            .evaluate_container_access(&ContainerMetadata {
                id: "cid-x".into(),
                names: vec!["other-db".into()],
                image: Some("postgres:16".into()),
                managed: false,
            })
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_CONTAINER_ALLOWLIST);
    }

    #[test]
    fn denies_container_denylist_entry() {
        let denial = policy()
            .evaluate_container_access(&ContainerMetadata {
                id: "cid-deny".into(),
                names: vec!["blocked-db".into()],
                image: Some("postgres:16".into()),
                managed: false,
            })
            .unwrap_err();
        assert_eq!(denial.rule_id, RULE_CONTAINER_DENYLIST);
    }

    #[test]
    fn allows_managed_container_without_explicit_entry() {
        assert!(policy()
            .evaluate_container_access(&ContainerMetadata {
                id: "cid-managed".into(),
                names: vec!["managed-db".into()],
                image: None,
                managed: true,
            })
            .is_ok());
    }

    #[test]
    fn allows_non_managed_container_when_name_is_allowlisted() {
        assert!(policy()
            .evaluate_container_access(&ContainerMetadata {
                id: "cid-other".into(),
                names: vec!["shared-db".into()],
                image: Some("postgres:16".into()),
                managed: false,
            })
            .is_ok());
    }

    #[test]
    fn normalize_bind_path_resolves_traversal() {
        assert_eq!(normalize_bind_path("/workspace/../etc/shadow"), "/etc/shadow");
        assert_eq!(normalize_bind_path("/workspace/./sub"), "/workspace/sub");
        assert_eq!(normalize_bind_path("/a/b/c/../../d"), "/a/d");
    }

    #[test]
    fn normalize_image_ref_expands_short_names() {
        assert_eq!(
            normalize_image_ref("postgres:16"),
            "docker.io/library/postgres:16"
        );
        assert_eq!(
            normalize_image_ref("myuser/myimage"),
            "docker.io/myuser/myimage:latest"
        );
        assert_eq!(
            normalize_image_ref("ghcr.io/org/image:v1"),
            "ghcr.io/org/image:v1"
        );
        assert_eq!(
            normalize_image_ref("postgres"),
            "docker.io/library/postgres:latest"
        );
    }

    #[test]
    fn saves_policy_with_new_fields() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("policy.json");
        let policy = policy();
        policy.save(&path).unwrap();
        let loaded = Policy::load(&path).unwrap();
        let mut allowlist = loaded.containers.allowlist;
        allowlist.sort();
        assert_eq!(allowlist, vec!["cid-allow", "shared-db"]);
    }
}
