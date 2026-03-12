use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::body::Bytes;
use http::HeaderMap;
use serde_json::{Map, Value};

pub const SESSION_HEADER: &str = "x-psp-session-id";
pub const EFFECTIVE_SESSION_HEADER: &str = "x-psp-effective-session-id";
const MAX_SESSION_ID_LEN: usize = 128;

fn sanitize_session_id(raw: &str) -> &str {
    let trimmed = raw.trim();
    let truncated = if trimmed.len() > MAX_SESSION_ID_LEN {
        &trimmed[..trimmed.floor_char_boundary(MAX_SESSION_ID_LEN)]
    } else {
        trimmed
    };
    if truncated.is_empty()
        || !truncated
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        "anonymous"
    } else {
        truncated
    }
}

pub const LABEL_MANAGED: &str = "io.psp.managed";
pub const LABEL_SESSION: &str = "io.psp.session";

#[derive(Clone, Debug)]
pub struct SessionManager {
    tracked: Arc<Mutex<SessionState>>,
    keep_on_failure: bool,
}

#[derive(Debug, Default)]
struct SessionState {
    containers: HashMap<String, String>, // container_id -> session_id
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionContext {
    pub raw: Option<String>,
    pub effective: String,
    pub provided: bool,
    pub valid: bool,
}

impl SessionContext {
    pub fn is_anonymous(&self) -> bool {
        self.effective == "anonymous"
    }
}

impl SessionManager {
    pub fn new(keep_on_failure: bool) -> Self {
        Self {
            tracked: Arc::new(Mutex::new(SessionState::default())),
            keep_on_failure,
        }
    }

    pub fn session_context(&self, headers: &HeaderMap) -> SessionContext {
        let raw = headers
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        // Warn if the session ID will be truncated
        if let Some(ref r) = raw {
            let trimmed = r.trim();
            if trimmed.len() > MAX_SESSION_ID_LEN {
                tracing::warn!(
                    provided_len = trimmed.len(),
                    max_len = MAX_SESSION_ID_LEN,
                    "session ID exceeds maximum length and will be truncated; use a shorter identifier"
                );
            }
        }
        let effective = raw
            .as_deref()
            .map(sanitize_session_id)
            .unwrap_or("anonymous")
            .to_string();
        let provided = raw.is_some();
        let valid = effective != "anonymous";

        SessionContext {
            raw,
            effective,
            provided,
            valid,
        }
    }

    pub fn session_id(&self, headers: &HeaderMap) -> String {
        self.session_context(headers).effective
    }

    pub fn track_container(&self, session: &str, id: &str) {
        let mut state = self.tracked.lock().unwrap();
        state.containers.insert(id.to_string(), session.to_string());
    }

    pub fn untrack_container(&self, id: &str) {
        let mut state = self.tracked.lock().unwrap();
        state.containers.remove(id);
    }

    pub fn is_tracked(&self, id: &str) -> bool {
        let state = self.tracked.lock().unwrap();
        state.containers.contains_key(id)
    }

    pub fn tracked_container_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = {
            let state = self.tracked.lock().unwrap();
            state.containers.keys().cloned().collect()
        };
        ids.sort();
        ids
    }

    pub fn keep_on_failure(&self) -> bool {
        self.keep_on_failure
    }
}

pub fn inject_session_labels(body: Bytes, session: &str) -> Result<Bytes> {
    if body.is_empty() {
        return Ok(body);
    }

    let value: Value = serde_json::from_slice(&body)?;
    inject_session_labels_value(value, session)
}

pub fn inject_session_labels_value(mut value: Value, session: &str) -> Result<Bytes> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("container create payload must be a JSON object"))?;
    let labels = object
        .entry("Labels")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Labels must be a JSON object"))?;
    labels.insert(LABEL_MANAGED.to_string(), Value::String("true".to_string()));
    labels.insert(
        LABEL_SESSION.to_string(),
        Value::String(session.to_string()),
    );
    Ok(Bytes::from(serde_json::to_vec(&value)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_valid_session_id() {
        assert_eq!(sanitize_session_id("sess-123"), "sess-123");
        assert_eq!(sanitize_session_id("my_session.v2"), "my_session.v2");
    }

    #[test]
    fn rejects_invalid_session_id_chars() {
        assert_eq!(sanitize_session_id("sess\n123"), "anonymous");
        assert_eq!(sanitize_session_id("sess\0id"), "anonymous");
        assert_eq!(sanitize_session_id("sess id"), "anonymous");
        assert_eq!(sanitize_session_id(""), "anonymous");
    }

    #[test]
    fn truncates_long_session_id() {
        let long_id = "a".repeat(200);
        let result = sanitize_session_id(&long_id);
        assert!(result.len() <= MAX_SESSION_ID_LEN);
        assert_ne!(result, "anonymous");
    }

    #[test]
    fn describes_missing_session_context() {
        let manager = SessionManager::new(false);
        let ctx = manager.session_context(&HeaderMap::new());
        assert_eq!(ctx.effective, "anonymous");
        assert!(!ctx.provided);
        assert!(!ctx.valid);
    }
}
