use axum::{body::Bytes, http::{Method, StatusCode}};

/// Rewrite response body. `normalized` must be a pre-normalized path.
pub fn rewrite_response_body(
    method: &Method,
    normalized: &str,
    status: StatusCode,
    advertised_host: &str,
    body: Bytes,
) -> Bytes {
    if method != Method::GET || status != StatusCode::OK {
        return body;
    }

    if !(normalized.starts_with("/containers/") && normalized.ends_with("/json")) {
        return body;
    }

    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };

    let Some(ports) = json
        .get_mut("NetworkSettings")
        .and_then(|network_settings| network_settings.get_mut("Ports"))
        .and_then(|ports| ports.as_object_mut())
    else {
        return body;
    };

    for entries in ports.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        for entry in entries {
            if let Some(host_ip) = entry.get_mut("HostIp") {
                *host_ip = serde_json::Value::String(advertised_host.to_string());
            }
        }
    }

    serde_json::to_vec(&json).map(Bytes::from).unwrap_or(body)
}
