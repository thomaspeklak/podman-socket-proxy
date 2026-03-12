use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{Method, Response, StatusCode},
};
use http_body_util::{BodyExt, Limited};
use tracing::{info, warn};

use crate::{
    AppState,
    audit::RequestAuditContext,
    backend::hop_by_hop_header,
    error::{ProxyError, with_context_headers},
    paths::{container_id_from_path, container_ref_from_path, is_supported_endpoint, normalize_versioned_path},
    rewrite::rewrite_response_body,
    session::inject_session_labels_value,
};

const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

pub async fn proxy_request(State(state): State<Arc<AppState>>, request: Request) -> Response<Body> {
    let request_id = state.next_request_id();
    let normalized = normalize_versioned_path(request.uri().path());
    let session = state.sessions.session_context(request.headers());

    if state.require_session_id && is_mutating(request.method()) && !session.valid {
        warn!(
            decision = "deny",
            kind = "session_required",
            request_id = %request_id,
            session = %session.effective,
            method = %request.method(),
            path = %request.uri().path(),
            "psp denied mutating request without a valid session id"
        );
        return ProxyError::session_required().into_response(&request_id, Some(&session.effective));
    }

    if !session.valid && is_mutating(request.method()) {
        warn!(
            decision = "warn",
            kind = "anonymous_session",
            request_id = %request_id,
            method = %request.method(),
            path = %request.uri().path(),
            "psp received a mutating request without a valid session id"
        );
    }

    if !is_supported_endpoint(request.method(), &normalized) {
        let audit = RequestAuditContext::from_request(
            request.method(),
            &normalized,
            request.uri().path(),
            request.uri().query(),
            None,
            session.effective.clone(),
        );
        warn!(
            decision = "deny",
            kind = "unsupported_endpoint",
            request_id = %request_id,
            session = %audit.session_id,
            operation = %audit.operation,
            path = %audit.path,
            target_image = audit.target_image.as_deref().unwrap_or(""),
            target_container = audit.target_container.as_deref().unwrap_or(""),
            "psp denied request"
        );
        return ProxyError::unsupported(request.method().clone(), request.uri().path())
            .into_response(&request_id, Some(&session.effective));
    }

    let (parts, body) = request.into_parts();

    if let Some(cl) = parts.headers.get(http::header::CONTENT_LENGTH)
        && let Ok(len) = cl.to_str().unwrap_or("0").parse::<usize>()
        && len > MAX_REQUEST_BODY_BYTES
    {
        return ProxyError::payload_too_large().into_response(&request_id, Some(&session.effective));
    }

    let limited = Limited::new(body, MAX_REQUEST_BODY_BYTES);
    let body = match limited.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            let proxy_error = if error.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                ProxyError::payload_too_large()
            } else {
                ProxyError::internal(anyhow::anyhow!("{error}"))
            };
            return proxy_error.into_response(&request_id, Some(&session.effective));
        }
    };

    let parsed_body: Option<serde_json::Value> = if parts.method == Method::POST
        && normalized == "/containers/create"
        && !body.is_empty()
    {
        serde_json::from_slice(&body).ok()
    } else {
        None
    };
    let audit = RequestAuditContext::from_request(
        &parts.method,
        &normalized,
        parts.uri.path(),
        parts.uri.query(),
        parsed_body.as_ref(),
        session.effective.clone(),
    );

    if let Some(container_ref) = container_ref_from_path(&normalized)
        && normalized != "/containers/create"
        && !state.sessions.is_tracked(container_ref)
    {
        match state.backend.inspect_container_metadata(container_ref).await {
            Ok(Some(container)) => {
                if let Err(denial) = state.policy.evaluate_container_access(&container) {
                    warn!(
                        decision = "deny",
                        kind = "policy_denied",
                        rule_id = denial.rule_id,
                        request_id = %request_id,
                        session = %audit.session_id,
                        operation = %audit.operation,
                        path = %audit.path,
                        target_image = audit.target_image.as_deref().unwrap_or(""),
                        target_container = container.display_name(),
                        reason = %denial.reason,
                        "psp denied request"
                    );
                    return ProxyError::policy_denied(denial)
                        .into_response(&request_id, Some(&session.effective));
                }
            }
            Ok(None) => {}
            Err(error) => {
                return error.into_response(&request_id, Some(&session.effective));
            }
        }
    }

    if let Err(denial) = state.policy.evaluate_request(
        &parts.method,
        &normalized,
        parts.uri.query(),
        body.as_ref(),
    ) {
        warn!(
            decision = "deny",
            kind = "policy_denied",
            rule_id = denial.rule_id,
            request_id = %request_id,
            session = %audit.session_id,
            operation = %audit.operation,
            path = %audit.path,
            target_image = audit.target_image.as_deref().unwrap_or(""),
            target_container = audit.target_container.as_deref().unwrap_or(""),
            reason = %denial.reason,
            "psp denied request"
        );
        return ProxyError::policy_denied(denial).into_response(&request_id, Some(&session.effective));
    }

    let body = if let Some(parsed) = parsed_body {
        match inject_session_labels_value(parsed, &session.effective) {
            Ok(body) => body,
            Err(error) => {
                return ProxyError::internal(error).into_response(&request_id, Some(&session.effective));
            }
        }
    } else {
        body
    };

    let method = parts.method.clone();
    let upstream = match state
        .backend
        .send(parts.method, &parts.uri, &parts.headers, body)
        .await
    {
        Ok(response) => response,
        Err(error) => return error.into_response(&request_id, Some(&session.effective)),
    };

    if method == Method::POST && normalized == "/containers/create" && upstream.status == StatusCode::CREATED
        && let Some(id) = extract_container_id(&upstream.body)
    {
        state.sessions.track_container(&session.effective, &id);
    }

    if method == Method::DELETE
        && normalized.starts_with("/containers/")
        && upstream.status.is_success()
        && let Some(id) = container_id_from_path(&normalized)
    {
        state.sessions.untrack_container(id);
    }

    info!(
        decision = "allow",
        request_id = %request_id,
        session = %audit.session_id,
        operation = %audit.operation,
        path = %audit.path,
        target_image = audit.target_image.as_deref().unwrap_or(""),
        target_container = audit.target_container.as_deref().unwrap_or(""),
        status = upstream.status.as_u16(),
        "psp forwarded request"
    );

    let body = rewrite_response_body(
        &method,
        &normalized,
        upstream.status,
        &state.advertised_host,
        upstream.body,
    );

    let mut response = Response::builder().status(upstream.status);
    for (name, value) in &upstream.headers {
        if hop_by_hop_header(name) || name == http::header::CONTENT_LENGTH {
            continue;
        }
        response = response.header(name, value);
    }
    response = response.header(http::header::CONTENT_LENGTH, body.len());

    match response.body(Body::from(body)) {
        Ok(response) => with_context_headers(response, &request_id, Some(&session.effective)),
        Err(error) => ProxyError::internal(error).into_response(&request_id, Some(&session.effective)),
    }
}

fn extract_container_id(body: &axum::body::Bytes) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|json| json.get("Id").and_then(|value| value.as_str()).map(str::to_string))
}

fn is_mutating(method: &Method) -> bool {
    !matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS)
}
