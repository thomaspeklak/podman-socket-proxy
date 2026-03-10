use std::{fs, path::Path};

use anyhow::{Result, bail};
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::form_urlencoded;

use crate::normalize_versioned_path;

pub const POLICY_SCHEMA_VERSION: &str = "v1";
pub const RULE_PRIVILEGED: &str = "PSP-POL-001";
pub const RULE_HOST_NAMESPACE: &str = "PSP-POL-002";
pub const RULE_BIND_MOUNT: &str = "PSP-POL-003";
pub const RULE_DEVICE_MOUNT: &str = "PSP-POL-004";
pub const RULE_CAP_ADD: &str = "PSP-POL-005";
pub const RULE_IMAGE_DENYLIST: &str = "PSP-POL-006";
pub const RULE_IMAGE_ALLOWLIST: &str = "PSP-POL-007";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
    pub version: String,
    #[serde(default)]
    pub bind_mounts: BindMountPolicy,
    #[serde(default)]
    pub images: ImagePolicy,
}

impl Policy {
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let policy: Policy = serde_json::from_str(&content)?;
        policy.validate()?;
        Ok(policy)
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

    pub fn evaluate_request(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        body: &[u8],
    ) -> Result<(), Denial> {
        let normalized = normalize_versioned_path(path);
        match (method.as_str(), normalized.as_str()) {
            ("POST", "/containers/create") => self.evaluate_container_create(body),
            ("POST", "/images/create") => self.evaluate_image_pull(query),
            _ => Ok(()),
        }
    }

    fn evaluate_container_create(&self, body: &[u8]) -> Result<(), Denial> {
        let request: ContainerCreateRequest = serde_json::from_slice(body).map_err(|error| {
            Denial::new(
                "PSP-POL-000",
                format!("invalid container create payload: {error}"),
            )
        })?;

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

    fn evaluate_image_reference(&self, image: &str) -> Result<(), Denial> {
        if self.images.denylist.iter().any(|entry| image == entry) {
            return Err(Denial::new(
                RULE_IMAGE_DENYLIST,
                format!("image is explicitly denied by policy: {image}"),
            ));
        }

        if !self.images.allowlist.is_empty()
            && !self.images.allowlist.iter().any(|entry| image == entry)
        {
            return Err(Denial::new(
                RULE_IMAGE_ALLOWLIST,
                format!("image is not present in the allowlist: {image}"),
            ));
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BindMountPolicy {
    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl BindMountPolicy {
    fn is_allowed(&self, source: &str) -> bool {
        !source.is_empty()
            && self
                .allowlist
                .iter()
                .any(|prefix| source == prefix || source.starts_with(&format!("{prefix}/")))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImagePolicy {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub denylist: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    pub rule_id: &'static str,
    pub reason: String,
}

impl Denial {
    fn new(rule_id: &'static str, reason: impl Into<String>) -> Self {
        Self {
            rule_id,
            reason: reason.into(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> Policy {
        Policy {
            version: POLICY_SCHEMA_VERSION.to_string(),
            bind_mounts: BindMountPolicy {
                allowlist: vec!["/workspace".into()],
            },
            images: ImagePolicy {
                allowlist: vec!["postgres:16".into(), "redis:7".into()],
                denylist: vec!["alpine:latest".into()],
            },
        }
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
                .evaluate_request(&Method::POST, "/v1.41/containers/create", None, &body)
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
}
