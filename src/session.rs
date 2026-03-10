use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::body::Bytes;
use http::HeaderMap;
use serde_json::{Map, Value};

pub const SESSION_HEADER: &str = "x-psp-session-id";
pub const LABEL_MANAGED: &str = "io.psp.managed";
pub const LABEL_SESSION: &str = "io.psp.session";

#[derive(Clone, Debug)]
pub struct SessionManager {
    tracked: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    keep_on_failure: bool,
}

impl SessionManager {
    pub fn new(keep_on_failure: bool) -> Self {
        Self {
            tracked: Arc::new(Mutex::new(HashMap::new())),
            keep_on_failure,
        }
    }

    pub fn session_id(&self, headers: &HeaderMap) -> String {
        headers
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("anonymous")
            .to_string()
    }

    pub fn track_container(&self, session: &str, id: &str) {
        let mut tracked = self.tracked.lock().unwrap();
        tracked
            .entry(session.to_string())
            .or_default()
            .insert(id.to_string());
    }

    pub fn untrack_container(&self, id: &str) {
        let mut tracked = self.tracked.lock().unwrap();
        for ids in tracked.values_mut() {
            ids.remove(id);
        }
    }

    pub fn tracked_container_ids(&self) -> Vec<String> {
        let tracked = self.tracked.lock().unwrap();
        let mut ids: Vec<_> = tracked
            .values()
            .flat_map(|ids| ids.iter().cloned())
            .collect();
        ids.sort();
        ids.dedup();
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

    let mut value: Value = serde_json::from_slice(&body)?;
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
