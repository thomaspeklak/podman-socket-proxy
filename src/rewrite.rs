use axum::{
    body::Bytes,
    http::{Method, StatusCode},
};

use crate::session::LABEL_MANAGED;

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

    // Container list — filter to PSP-managed containers only.
    if normalized == "/containers/json" {
        return filter_managed_containers(body);
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

/// Keep only containers that are either PSP-managed or created by Testcontainers.
///
/// PSP-managed containers carry `io.psp.managed=true`. Testcontainers containers
/// carry `org.testcontainers=true`. Both need to be visible so the reuse flow
/// (e.g. REBUILD_SNAPSHOT=false) can find containers created in previous sessions
/// that were not themselves started through this PSP instance.
fn filter_managed_containers(body: Bytes) -> Bytes {
    let Ok(serde_json::Value::Array(containers)) =
        serde_json::from_slice::<serde_json::Value>(&body)
    else {
        return body;
    };

    let filtered: Vec<serde_json::Value> = containers
        .into_iter()
        .filter(|c| {
            let labels = c.get("Labels");
            let psp_managed = labels
                .and_then(|l| l.get(LABEL_MANAGED))
                .and_then(|v| v.as_str())
                == Some("true");
            let testcontainers = labels
                .and_then(|l| l.get("org.testcontainers"))
                .and_then(|v| v.as_str())
                == Some("true");
            psp_managed || testcontainers
        })
        .collect();

    serde_json::to_vec(&filtered)
        .map(Bytes::from)
        .unwrap_or(body)
}
