use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use axum::{
    body::Bytes,
    http::{Method, StatusCode, Uri},
};
use http_body_util::{BodyExt, Full};
use hyper::Request as HyperRequest;
use hyper_util::{client::legacy::Client as HyperClient, rt::TokioExecutor};
use hyperlocal::UnixConnector;
use reqwest::Client as ReqwestClient;
use url::Url;

use axum::http::HeaderName;

use crate::config::BackendConfig;
use crate::error::ProxyError;
use crate::policy::ContainerMetadata;
use crate::session::LABEL_MANAGED;

const BACKEND_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

#[derive(Clone)]
pub enum BackendClient {
    Http {
        client: ReqwestClient,
        base: Url,
    },
    Unix {
        client: HyperClient<UnixConnector, Full<Bytes>>,
        socket: PathBuf,
    },
}

impl BackendClient {
    pub fn new(config: BackendConfig) -> Result<Self> {
        Ok(match config {
            BackendConfig::Http(base) => Self::Http {
                client: ReqwestClient::builder().timeout(BACKEND_TIMEOUT).build()?,
                base,
            },
            BackendConfig::Unix(socket) => Self::Unix {
                client: HyperClient::builder(TokioExecutor::new()).build(UnixConnector),
                socket,
            },
        })
    }

    pub async fn send(
        &self,
        method: Method,
        uri: &Uri,
        headers: &http::HeaderMap,
        body: Bytes,
    ) -> Result<ForwardedResponse, ProxyError> {
        let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

        let mut filtered_headers = Vec::new();
        for (name, value) in headers {
            if name.as_str().eq_ignore_ascii_case("host") || name == http::header::CONTENT_LENGTH {
                continue;
            }
            filtered_headers.push((name.clone(), value.clone()));
        }

        match self {
            Self::Http { client, base } => {
                let url = base
                    .join(path_and_query.trim_start_matches('/'))
                    .map_err(ProxyError::internal)?;
                let mut upstream = client.request(method, url);
                for (name, value) in &filtered_headers {
                    upstream = upstream.header(name, value);
                }
                let response = upstream.body(body).send().await.map_err(ProxyError::backend)?;

                if let Some(cl) = response.content_length()
                    && cl > MAX_RESPONSE_BODY_BYTES as u64
                {
                    return Err(ProxyError::Backend(
                        "backend response body too large".to_string(),
                    ));
                }

                let status = response.status();
                let headers = response.headers().clone();
                let body = response.bytes().await.map_err(ProxyError::backend)?;
                if body.len() > MAX_RESPONSE_BODY_BYTES {
                    return Err(ProxyError::Backend(
                        "backend response body too large".to_string(),
                    ));
                }
                Ok(ForwardedResponse {
                    status,
                    headers,
                    body,
                })
            }
            Self::Unix { client, socket } => {
                let upstream_uri: hyper::Uri = hyperlocal::Uri::new(socket, path_and_query).into();
                let mut request_builder = HyperRequest::builder().method(method).uri(upstream_uri);
                for (name, value) in &filtered_headers {
                    request_builder = request_builder.header(name, value);
                }
                let request = request_builder
                    .body(Full::new(body))
                    .map_err(ProxyError::internal)?;

                let (parts, body) = tokio::time::timeout(BACKEND_TIMEOUT, async {
                    let response = client
                        .request(request)
                        .await
                        .map_err(ProxyError::hyper_backend)?;
                    let (parts, body) = response.into_parts();
                    let limited = http_body_util::Limited::new(body, MAX_RESPONSE_BODY_BYTES);
                    let body = limited
                        .collect()
                        .await
                        .map_err(|e| {
                            if e.is::<http_body_util::LengthLimitError>() {
                                ProxyError::Backend("backend response body too large".to_string())
                            } else {
                                ProxyError::hyper_backend(e)
                            }
                        })?
                        .to_bytes();
                    Ok::<_, ProxyError>((parts, body))
                })
                .await
                .map_err(|_| ProxyError::backend_timeout())??;

                Ok(ForwardedResponse {
                    status: parts.status,
                    headers: parts.headers,
                    body,
                })
            }
        }
    }

    pub async fn get_json(&self, path_and_query: &str) -> Result<serde_json::Value, ProxyError> {
        let uri: Uri = path_and_query.parse().map_err(ProxyError::internal)?;
        let response = self
            .send(Method::GET, &uri, &http::HeaderMap::new(), Bytes::new())
            .await?;
        serde_json::from_slice(&response.body).map_err(ProxyError::internal)
    }

    pub async fn get_text(&self, path_and_query: &str) -> Result<String, ProxyError> {
        let uri: Uri = path_and_query.parse().map_err(ProxyError::internal)?;
        let response = self
            .send(Method::GET, &uri, &http::HeaderMap::new(), Bytes::new())
            .await?;
        String::from_utf8(response.body.to_vec()).map_err(ProxyError::internal)
    }

    pub async fn delete(&self, path_and_query: &str) -> Result<(), ProxyError> {
        let uri: Uri = path_and_query.parse().map_err(ProxyError::internal)?;
        self.send(Method::DELETE, &uri, &http::HeaderMap::new(), Bytes::new())
            .await?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<(), ProxyError> {
        let response = self.get_text("/_ping").await?;
        if response.trim() == "OK" {
            Ok(())
        } else {
            Err(ProxyError::internal(anyhow!(
                "unexpected backend ping response: {response}"
            )))
        }
    }

    pub async fn version(&self) -> Result<serde_json::Value, ProxyError> {
        self.get_json("/version").await
    }

    pub async fn inspect_container_metadata(
        &self,
        container_ref: &str,
    ) -> Result<Option<ContainerMetadata>, ProxyError> {
        let uri: Uri = format!("/containers/{container_ref}/json")
            .parse()
            .map_err(ProxyError::internal)?;
        let response = self
            .send(Method::GET, &uri, &http::HeaderMap::new(), Bytes::new())
            .await?;
        if response.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status.is_success() {
            return Err(ProxyError::Backend(format!(
                "container inspect failed with status {}",
                response.status
            )));
        }
        let json = serde_json::from_slice(&response.body).map_err(ProxyError::internal)?;
        container_metadata_from_inspect(&json).map(Some)
    }

    pub async fn list_containers(&self, all: bool) -> Result<Vec<DiscoveredContainer>, ProxyError> {
        let all_flag = if all { 1 } else { 0 };
        let json = self.get_json(&format!("/containers/json?all={all_flag}")).await?;
        let containers = json
            .as_array()
            .ok_or_else(|| ProxyError::internal(anyhow!("backend /containers/json did not return an array")))?;

        containers
            .iter()
            .map(discovered_container_from_summary)
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ForwardedResponse {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

#[derive(Clone, Debug)]
pub struct DiscoveredContainer {
    pub metadata: ContainerMetadata,
    pub state: Option<String>,
    pub status: Option<String>,
}

pub(crate) fn hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn discovered_container_from_summary(value: &serde_json::Value) -> Result<DiscoveredContainer, ProxyError> {
    let id = value
        .get("Id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProxyError::internal(anyhow!("container summary missing Id")))?
        .to_string();

    let names = value
        .get("Names")
        .and_then(|v| v.as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str())
                .map(|name| name.trim_start_matches('/').to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let image = value.get("Image").and_then(|v| v.as_str()).map(str::to_string);
    let managed = value
        .get("Labels")
        .and_then(|v| v.get(LABEL_MANAGED))
        .and_then(|v| v.as_str())
        .map(|value| value == "true")
        .unwrap_or(false);

    Ok(DiscoveredContainer {
        metadata: ContainerMetadata {
            id,
            names,
            image,
            managed,
        },
        state: value.get("State").and_then(|v| v.as_str()).map(str::to_string),
        status: value.get("Status").and_then(|v| v.as_str()).map(str::to_string),
    })
}

fn container_metadata_from_inspect(value: &serde_json::Value) -> Result<ContainerMetadata, ProxyError> {
    let id = value
        .get("Id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProxyError::internal(anyhow!("container inspect response missing Id")))?
        .to_string();

    let mut names = Vec::new();
    if let Some(name) = value.get("Name").and_then(|v| v.as_str()) {
        let normalized = name.trim_start_matches('/');
        if !normalized.is_empty() {
            names.push(normalized.to_string());
        }
    }

    let image = value
        .get("Config")
        .and_then(|v| v.get("Image"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let managed = value
        .get("Config")
        .and_then(|v| v.get("Labels"))
        .and_then(|v| v.get(LABEL_MANAGED))
        .and_then(|v| v.as_str())
        .map(|value| value == "true")
        .unwrap_or(false);

    Ok(ContainerMetadata {
        id,
        names,
        image,
        managed,
    })
}
