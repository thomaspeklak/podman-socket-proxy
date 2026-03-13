use axum::{
    body::Body,
    http::{HeaderValue, Method, Response, StatusCode},
};
use serde::Serialize;
use tracing::error;

use crate::{
    policy::{
        Denial, RULE_BIND_MOUNT, RULE_CAP_ADD, RULE_CONTAINER_ALLOWLIST, RULE_CONTAINER_DENYLIST,
        RULE_DEVICE_MOUNT, RULE_HOST_NAMESPACE, RULE_IMAGE_ALLOWLIST, RULE_IMAGE_DENYLIST,
        RULE_PARSE_ERROR, RULE_PRIVILEGED,
    },
    session::EFFECTIVE_SESSION_HEADER,
};

pub const REQUEST_ID_HEADER: &str = "x-psp-request-id";

#[derive(Debug)]
pub enum ProxyError {
    Unsupported { method: Method, path: String },
    PolicyDenied(Denial),
    PayloadTooLarge,
    Backend(String),
    BackendTimeout,
    SessionRequired,
    Internal(anyhow::Error),
}

impl ProxyError {
    pub fn unsupported(method: Method, path: &str) -> Self {
        Self::Unsupported {
            method,
            path: path.to_string(),
        }
    }

    pub fn policy_denied(denial: Denial) -> Self {
        Self::PolicyDenied(denial)
    }

    pub fn payload_too_large() -> Self {
        Self::PayloadTooLarge
    }

    pub fn backend(error: reqwest::Error) -> Self {
        Self::Backend(error.to_string())
    }

    pub fn hyper_backend<E: std::fmt::Display>(error: E) -> Self {
        Self::Backend(error.to_string())
    }

    pub fn backend_timeout() -> Self {
        Self::BackendTimeout
    }

    pub fn session_required() -> Self {
        Self::SessionRequired
    }

    pub fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }

    pub fn into_response(self, request_id: &str, session_id: Option<&str>) -> Response<Body> {
        let response = match self {
            Self::Unsupported { method, path } => json_response(
                StatusCode::NOT_IMPLEMENTED,
                &ErrorBody {
                    message: format!("unsupported endpoint: {} {}", method, path),
                    kind: "unsupported_endpoint",
                    method: Some(method.to_string()),
                    path: Some(path),
                    rule_id: None,
                    hint: Some("Use only the documented Testcontainers-compatible PSP API subset."),
                    docs: Some("docs/compatibility/testcontainers-profile.md"),
                    request_id: request_id.to_string(),
                    session_id: session_id.map(str::to_string),
                },
            ),
            Self::PolicyDenied(denial) => {
                let (hint, docs) = denial_metadata(denial.rule_id);
                json_response(
                    StatusCode::FORBIDDEN,
                    &ErrorBody {
                        message: denial.reason,
                        kind: "policy_denied",
                        method: None,
                        path: None,
                        rule_id: Some(denial.rule_id),
                        hint,
                        docs,
                        request_id: request_id.to_string(),
                        session_id: session_id.map(str::to_string),
                    },
                )
            }
            Self::PayloadTooLarge => json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ErrorBody {
                    message: "request body exceeds maximum allowed size".to_string(),
                    kind: "payload_too_large",
                    method: None,
                    path: None,
                    rule_id: None,
                    hint: Some("Reduce the request size or avoid sending large inline payloads."),
                    docs: None,
                    request_id: request_id.to_string(),
                    session_id: session_id.map(str::to_string),
                },
            ),
            Self::Backend(message) => json_response(
                StatusCode::BAD_GATEWAY,
                &ErrorBody {
                    message: format!("backend request failed: {message}"),
                    kind: "backend_error",
                    method: None,
                    path: None,
                    rule_id: None,
                    hint: Some(
                        "Verify Podman is reachable and the configured backend endpoint is correct.",
                    ),
                    docs: Some("docs/operations/runbook.md"),
                    request_id: request_id.to_string(),
                    session_id: session_id.map(str::to_string),
                },
            ),
            Self::BackendTimeout => json_response(
                StatusCode::GATEWAY_TIMEOUT,
                &ErrorBody {
                    message: "backend request timed out".to_string(),
                    kind: "backend_timeout",
                    method: None,
                    path: None,
                    rule_id: None,
                    hint: Some(
                        "Check backend health and whether the requested container operation is hanging.",
                    ),
                    docs: Some("docs/operations/runbook.md"),
                    request_id: request_id.to_string(),
                    session_id: session_id.map(str::to_string),
                },
            ),
            Self::SessionRequired => json_response(
                StatusCode::BAD_REQUEST,
                &ErrorBody {
                    message: "a valid x-psp-session-id header is required for mutating requests"
                        .to_string(),
                    kind: "session_required",
                    method: None,
                    path: None,
                    rule_id: None,
                    hint: Some(
                        "Attach a stable session ID such as a sandbox or test-run identifier.",
                    ),
                    docs: Some("docs/ags-integration.md"),
                    request_id: request_id.to_string(),
                    session_id: session_id.map(str::to_string),
                },
            ),
            Self::Internal(error_value) => {
                error!(error = ?error_value, request_id, "internal proxy error");
                json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorBody {
                        message: "internal proxy error".to_string(),
                        kind: "internal_error",
                        method: None,
                        path: None,
                        rule_id: None,
                        hint: Some("Check PSP logs using the returned request ID for correlation."),
                        docs: Some("docs/operations/audit-logging.md"),
                        request_id: request_id.to_string(),
                        session_id: session_id.map(str::to_string),
                    },
                )
            }
        };

        with_context_headers(response, request_id, session_id)
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: String,
    kind: &'a str,
    method: Option<String>,
    path: Option<String>,
    rule_id: Option<&'a str>,
    hint: Option<&'a str>,
    docs: Option<&'a str>,
    request_id: String,
    session_id: Option<String>,
}

pub fn json_response(status: StatusCode, body: &impl Serialize) -> Response<Body> {
    match serde_json::to_vec(body) {
        Ok(json) => Response::builder()
            .status(status)
            .header(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(json))
            .unwrap_or_else(|_| plain_500()),
        Err(_) => plain_500(),
    }
}

pub fn with_context_headers(
    mut response: Response<Body>,
    request_id: &str,
    session_id: Option<&str>,
) -> Response<Body> {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    if let Some(session_id) = session_id
        && let Ok(value) = HeaderValue::from_str(session_id)
    {
        response
            .headers_mut()
            .insert(EFFECTIVE_SESSION_HEADER, value);
    }
    response
}

fn denial_metadata(rule_id: &str) -> (Option<&'static str>, Option<&'static str>) {
    match rule_id {
        RULE_PARSE_ERROR => (
            Some("Send a valid Docker-compatible container create JSON payload."),
            Some("docs/examples/http-api-examples.md"),
        ),
        RULE_PRIVILEGED => (
            Some(
                "Remove HostConfig.Privileged or change policy intentionally if this is expected.",
            ),
            Some("docs/policy-reference.md"),
        ),
        RULE_HOST_NAMESPACE => (
            Some("Use isolated namespaces instead of host or container namespace joins."),
            Some("docs/policy-reference.md"),
        ),
        RULE_BIND_MOUNT => (
            Some("Allowlist only the narrow host path prefix required for this bind mount."),
            Some("docs/policy-reference.md"),
        ),
        RULE_DEVICE_MOUNT => (
            Some("Remove device mappings unless the runtime contract explicitly requires them."),
            Some("docs/policy-reference.md"),
        ),
        RULE_CAP_ADD => (
            Some("Drop CapAdd entries or extend policy only after review."),
            Some("docs/policy-reference.md"),
        ),
        RULE_IMAGE_DENYLIST => (
            Some("Choose a different image or remove the denylist entry intentionally."),
            Some("docs/policy-reference.md"),
        ),
        RULE_IMAGE_ALLOWLIST => (
            Some("Add the image to the allowlist through policy tooling if it is approved."),
            Some("docs/policy-reference.md"),
        ),
        RULE_CONTAINER_DENYLIST => (
            Some(
                "Remove the explicit container deny entry only if access is intentionally approved.",
            ),
            Some("docs/policy-reference.md"),
        ),
        RULE_CONTAINER_ALLOWLIST => (
            Some("Use discovery mode to allow access to this pre-existing container explicitly."),
            Some("docs/policy-reference.md"),
        ),
        _ => (None, Some("docs/policy-reference.md")),
    }
}

fn plain_500() -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        )
        .body(Body::from("internal proxy error"))
        .unwrap()
}
