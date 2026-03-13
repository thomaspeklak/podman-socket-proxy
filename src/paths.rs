use axum::http::Method;

/// Normalize a versioned API path by stripping the version prefix.
pub fn normalize_versioned_path(path: &str) -> String {
    let trimmed = if path.is_empty() { "/" } else { path };
    let segments: Vec<&str> = trimmed.trim_start_matches('/').split('/').collect();
    if let Some(first) = segments.first()
        && first.starts_with('v')
        && first[1..]
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch == '.')
        && segments.len() > 1
    {
        return format!("/{}", segments[1..].join("/"));
    }
    trimmed.to_string()
}

/// Check if the given method + already-normalized path is a supported endpoint.
pub fn is_supported_endpoint(method: &Method, normalized: &str) -> bool {
    match (method.as_str(), normalized) {
        ("GET", "/_ping") | ("GET", "/version") | ("GET", "/info") | ("POST", "/images/create") => {
            true
        }
        ("POST", "/containers/create") => true,
        _ if normalized.starts_with("/images/") && normalized.ends_with("/json") => {
            // GET /images/{name}/json — image inspect; name may contain slashes (org/image)
            method.as_str() == "GET" && path_segment_count(normalized) >= 3
        }
        _ if normalized.starts_with("/containers/") => {
            let segments = path_segment_count(normalized);
            match (method.as_str(), segments) {
                // container list — PSP filters response to managed containers only
                ("GET", 2) => normalized == "/containers/json",
                ("GET", 3) => normalized.ends_with("/json") || normalized.ends_with("/logs"),
                ("POST", 3) => {
                    normalized.ends_with("/start")
                        || normalized.ends_with("/stop")
                        || normalized.ends_with("/wait")
                        || normalized.ends_with("/exec")
                }
                ("PUT", 3) => normalized.ends_with("/archive"),

                ("DELETE", 2) => normalized != "/containers/json",
                _ => false,
            }
        }
        // exec start and inspect — always 3 segments: /exec/{id}/start or /exec/{id}/json
        _ if normalized.starts_with("/exec/") => {
            let segments = path_segment_count(normalized);
            matches!(
                (method.as_str(), segments),
                ("POST", 3) | ("GET", 3)
            ) && (normalized.ends_with("/start") || normalized.ends_with("/json"))
        }
        _ => false,
    }
}

pub fn path_segment_count(path: &str) -> usize {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .count()
}

/// Extract container ID from `/containers/{id}` (no sub-path). For DELETE tracking.
pub fn container_id_from_path(normalized_path: &str) -> Option<&str> {
    let trimmed = normalized_path.strip_prefix("/containers/")?;
    if trimmed.is_empty() || trimmed.contains('/') {
        None
    } else {
        Some(trimmed)
    }
}

/// Extract container reference from `/containers/{id}/...`. Returns None for `/containers/create`.
pub fn container_ref_from_path(normalized_path: &str) -> Option<&str> {
    let trimmed = normalized_path.strip_prefix("/containers/")?;
    let id = trimmed.split('/').next().filter(|s| !s.is_empty())?;
    if id == "create" { None } else { Some(id) }
}
