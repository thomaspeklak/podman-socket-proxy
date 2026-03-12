use axum::http::Method;
use serde_json::Value;

use crate::paths::container_ref_from_path;

#[derive(Clone, Debug, Default)]
pub struct RequestAuditContext {
    pub session_id: String,
    pub operation: String,
    pub path: String,
    pub target_image: Option<String>,
    pub target_container: Option<String>,
}

impl RequestAuditContext {
    pub fn from_request(
        method: &Method,
        normalized: &str,
        raw_path: &str,
        query: Option<&str>,
        parsed_body: Option<&Value>,
        session_id: String,
    ) -> Self {
        Self {
            session_id,
            operation: operation_name(method, normalized),
            path: raw_path.to_string(),
            target_image: extract_target_image(method, normalized, query, parsed_body),
            target_container: container_ref_from_path(normalized).map(str::to_string),
        }
    }
}

fn operation_name(method: &Method, normalized: &str) -> String {
    match (method.as_str(), normalized) {
        ("GET", "/_ping") => "daemon.ping".into(),
        ("GET", "/version") => "daemon.version".into(),
        ("GET", "/info") => "daemon.info".into(),
        ("POST", "/images/create") => "images.create".into(),
        ("POST", "/containers/create") => "containers.create".into(),
        _ if method == Method::POST && normalized.ends_with("/start") => "containers.start".into(),
        _ if method == Method::GET && normalized.ends_with("/json") => "containers.inspect".into(),
        _ if method == Method::GET && normalized.ends_with("/logs") => "containers.logs".into(),
        _ if method == Method::POST && normalized.ends_with("/wait") => "containers.wait".into(),
        _ if method == Method::DELETE && normalized.starts_with("/containers/") => {
            "containers.delete".into()
        }
        _ => format!("{} {}", method, normalized),
    }
}

fn extract_target_image(
    method: &Method,
    normalized: &str,
    query: Option<&str>,
    parsed_body: Option<&Value>,
) -> Option<String> {
    if method == Method::POST && normalized == "/images/create" {
        return query.and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "fromImage")
                .map(|(_, value)| value.into_owned())
        });
    }

    if method == Method::POST && normalized == "/containers/create" {
        return parsed_body
            .and_then(|json| json.get("Image"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    None
}
