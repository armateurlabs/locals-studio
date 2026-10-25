use crate::{service::Studio, types::ApiConfig};
use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::sync::oneshot;

pub struct GatewayHandle {
    pub description: Value,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl GatewayHandle {
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(std::time::Duration::from_secs(30), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

#[derive(Clone)]
struct GatewayState {
    studio: Arc<Studio>,
    policy: Arc<Policy>,
    client: reqwest::Client,
}

#[derive(Debug)]
struct Policy {
    hosts: HashSet<String>,
    origins: HashSet<String>,
}

impl Policy {
    fn new(config: &ApiConfig, addresses: &[String]) -> Self {
        let mut hosts: HashSet<_> = [
            "localhost".into(),
            "127.0.0.1".into(),
            "::1".into(),
            "tauri.localhost".into(),
        ]
        .into_iter()
        .collect();
        hosts.extend(addresses.iter().map(|s| s.to_ascii_lowercase()));
        if config.host != "0.0.0.0" && config.host != "::" {
            hosts.insert(config.host.to_ascii_lowercase());
        }
        hosts.extend(config.allowed_hosts.iter().map(|s| s.to_ascii_lowercase()));
        Self {
            hosts,
            origins: config
                .allowed_origins
                .iter()
                .map(|s| s.trim_end_matches('/').to_owned())
                .collect(),
        }
    }

    fn check(&self, headers: &HeaderMap) -> std::result::Result<Option<String>, String> {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or("Host header is required")?;
        let authority: axum::http::uri::Authority =
            host.parse().map_err(|_| "Invalid Host header")?;
        let hostname = authority
            .host()
            .trim_matches(['[', ']'])
            .to_ascii_lowercase();
        if !self.hosts.contains(&hostname) && !self.hosts.contains("*") {
            return Err("This Host is not allowed by Studio".into());
        }
        let Some(origin) = headers.get(header::ORIGIN) else {
            return Ok(None);
        };
        let origin = origin.to_str().map_err(|_| "Invalid Origin header")?;
        if origin == "null" {
            return Err("Opaque browser origins are not allowed".into());
        }
        let allowed = self.origins.contains(origin.trim_end_matches('/'))
            || self.origins.contains("*")
            || local_origin(origin);
        if !allowed {
            return Err(format!(
                "Browser origin {origin} is not allowed. Add its exact URL to api.allowed_origins"
            ));
        }
        Ok(Some(origin.to_owned()))
    }
}

fn local_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https" | "tauri")
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1" | "tauri.localhost")
        )
}

pub async fn start(studio: Arc<Studio>, config: ApiConfig) -> Result<GatewayHandle> {
    let address = SocketAddr::new(config.host.parse::<IpAddr>()?, config.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| {
            format!("Cannot bind Studio API to {address}; another server may already use this port")
        })?;
    let mut addresses = interface_addresses();
    addresses.push("127.0.0.1".into());
    addresses.push("::1".into());
    addresses.sort();
    addresses.dedup();
    let visible: Vec<_> = if config.host == "0.0.0.0" {
        addresses
            .iter()
            .filter(|v| !v.contains(':'))
            .cloned()
            .collect()
    } else if config.host == "::" {
        addresses.clone()
    } else {
        vec![config.host.clone()]
    };
    let urls: Vec<_> = visible
        .iter()
        .map(|h| {
            format!(
                "http://{}:{}/v1",
                if h.contains(':') {
                    format!("[{h}]")
                } else {
                    h.clone()
                },
                config.port
            )
        })
        .collect();
    let policy = Arc::new(Policy::new(&config, &addresses));
    let state = GatewayState {
        studio,
        policy,
        client: reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()?,
    };
    let router = Router::new().fallback(proxy).with_state(state);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
        {
            tracing::error!(%error,"Studio gateway stopped");
        }
    });
    Ok(GatewayHandle {
        description: json!({"running":true,"host":config.host,"port":config.port,"urls":urls,"lan":config.host!="127.0.0.1" && config.host!="::1"}),
        stop: Some(stop),
        task,
    })
}

fn interface_addresses() -> Vec<String> {
    let Ok(output) = std::process::Command::new("/sbin/ifconfig").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("inet") => parts.next().map(str::to_owned),
                Some("inet6") => parts.next().filter(|s| !s.contains('%')).map(str::to_owned),
                _ => None,
            }
        })
        .collect()
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(json!({"error":{"message":message.into(),"type":"studio_error"}})),
    )
        .into_response()
}

fn cors(response: &mut Response, origin: Option<&str>, requested_headers: Option<&HeaderValue>) {
    let headers = response.headers_mut();
    for name in [
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        header::ACCESS_CONTROL_ALLOW_METHODS,
    ] {
        headers.remove(name);
    }
    if let Some(origin) = origin {
        if let Ok(value) = HeaderValue::from_str(origin) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            requested_headers.cloned().unwrap_or_else(|| {
                HeaderValue::from_static(
                    "authorization, content-type, x-api-key, anthropic-version",
                )
            }),
        );
    }
}

async fn proxy(State(state): State<GatewayState>, request: Request) -> Response {
    let origin = match state.policy.check(request.headers()) {
        Ok(origin) => origin,
        Err(message) => return error(StatusCode::FORBIDDEN, message),
    };
    let requested_headers = request
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned();
    let mut response = if request.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        dispatch(state, request).await
    };
    cors(&mut response, origin.as_deref(), requested_headers.as_ref());
    response
}

async fn dispatch(state: GatewayState, request: Request) -> Response {
    let path = request.uri().path().trim_end_matches('/').to_owned();
    if request.method() == Method::GET {
        if path == "/health" {
            return axum::Json(json!({"status":"ok","service":"locals-studio"})).into_response();
        }
        if path == "/api/version" {
            return axum::Json(json!({"version":"0.12.6"})).into_response();
        }
        if path == "/" || path.is_empty() {
            return "Locals Studio API".into_response();
        }
        if matches!(
            path.as_str(),
            "/v1/models" | "/models" | "/api/tags" | "/api/ps"
        ) {
            let servers = state.studio.supervisor.list().await;
            let available: Vec<_> = servers
                .iter()
                .filter(|s| matches!(s.state.as_str(), "ready" | "running"))
                .collect();
            if path.starts_with("/api/") {
                let models:Vec<_>=available.iter().map(|s|json!({"name":s.alias,"model":s.alias,"modified_at":s.started_at,"size":0,"digest":s.model_id,"details":{"format":"mlx","family":"locals"}})).collect();
                return axum::Json(json!({"models":models})).into_response();
            }
            let data:Vec<_>=available.iter().map(|s|json!({"id":s.alias,"object":"model","owned_by":"locals-studio","created":0,"type":"model","display_name":s.alias})).collect();
            return axum::Json(json!({"object":"list","data":data,"has_more":false}))
                .into_response();
        }
        if let Some(alias) = path
            .strip_prefix("/v1/models/")
            .or_else(|| path.strip_prefix("/models/"))
        {
            if state
                .studio
                .supervisor
                .list()
                .await
                .iter()
                .any(|s| s.alias == alias && matches!(s.state.as_str(), "ready" | "running"))
            {
                return axum::Json(json!({"id":alias,"object":"model","owned_by":"locals-studio"}))
                    .into_response();
            }
            return error(StatusCode::NOT_FOUND, "This model is not running");
        }
        return error(StatusCode::NOT_FOUND, "Unknown inference endpoint");
    }
    if request.method() != Method::POST {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "The Studio inference API accepts GET, POST and OPTIONS",
        );
    }
    if path == "/api/pull" {
        return error(
            StatusCode::BAD_REQUEST,
            "Use Locals Studio's download manager or `locals-studio download add` to download models",
        );
    }
    if !matches!(
        path.as_str(),
        "/v1/chat/completions"
            | "/chat/completions"
            | "/v1/completions"
            | "/completions"
            | "/v1/messages"
            | "/v1/messages/count_tokens"
            | "/api/chat"
            | "/api/generate"
            | "/api/show"
            | "/api/embed"
            | "/api/embeddings"
    ) {
        return error(StatusCode::NOT_FOUND, "Unknown inference endpoint");
    }
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, 32 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Request exceeds 32 MiB"),
    };
    let mut value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Request must be a JSON object"),
    };
    if !value.is_object() {
        return error(StatusCode::BAD_REQUEST, "Request must be a JSON object");
    }
    let Some(requested) = value["model"].as_str().map(str::to_owned) else {
        return error(StatusCode::BAD_REQUEST, "model is required");
    };
    let (mut alias, engine, mut thinking) = parse_model(&requested);
    let undecorated = requested
        .strip_prefix("locals/")
        .or_else(|| requested.strip_prefix("mlx/"))
        .or_else(|| requested.strip_prefix("stock/"))
        .unwrap_or(&requested);
    if state
        .studio
        .store
        .list::<crate::types::ServerProfile>("profiles")
        .unwrap_or_default()
        .iter()
        .any(|p| p.alias == undecorated)
    {
        alias = undecorated.to_owned();
        thinking = false;
    }
    let lease = match state.studio.supervisor.acquire(&alias).await {
        Ok(lease) => lease,
        Err(failure) => return error(StatusCode::SERVICE_UNAVAILABLE, format!("{failure:#}")),
    };
    let canonical = rewrite_model(&lease.backend_model, engine, thinking);
    value["model"] = json!(canonical);
    let mut upstream = state
        .client
        .post(format!("{}{}", lease.base_url, path))
        .json(&value);
    // reqwest supplies the loopback Host. Client-supplied forwarded headers never reach workers.
    for name in [
        header::ORIGIN,
        header::AUTHORIZATION,
        header::ACCEPT,
        header::CONTENT_TYPE,
    ] {
        if let Some(value) = parts.headers.get(&name) {
            upstream = upstream.header(name, value);
        }
    }
    for name in ["anthropic-version", "anthropic-beta", "x-api-key"] {
        if let Some(value) = parts.headers.get(name) {
            upstream = upstream.header(name, value);
        }
    }
    let response = match upstream.send().await {
        Ok(response) => response,
        Err(failure) => {
            return error(
                StatusCode::BAD_GATEWAY,
                format!("Locals worker unavailable: {failure}"),
            );
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let stream = async_stream::stream! {
        let _lease=lease;
        let mut chunks=response.bytes_stream();
        while let Some(chunk)=chunks.next().await{yield chunk;}
    };
    let mut result = Response::new(Body::from_stream(stream));
    *result.status_mut() = status;
    for (name, value) in &headers {
        if !matches!(
            name.as_str(),
            "connection"
                | "transfer-encoding"
                | "keep-alive"
                | "upgrade"
                | "proxy-authenticate"
                | "proxy-authorization"
        ) && !name.as_str().starts_with("access-control-")
        {
            result.headers_mut().insert(name.clone(), value.clone());
        }
    }
    result
}

fn parse_model(requested: &str) -> (String, Option<&str>, bool) {
    let (name, engine) = if let Some(name) = requested
        .strip_prefix("mlx/")
        .or_else(|| requested.strip_prefix("stock/"))
    {
        (name, Some("mlx"))
    } else {
        (requested.strip_prefix("locals/").unwrap_or(requested), None)
    };
    if let Some(name) = name.strip_suffix("-thinking") {
        (name.into(), engine, true)
    } else {
        (name.into(), engine, false)
    }
}

fn rewrite_model(canonical: &str, engine: Option<&str>, thinking: bool) -> String {
    let canonical = canonical.strip_suffix("-thinking").unwrap_or(canonical);
    let name = if let Some(engine) = engine {
        format!(
            "{engine}/{}",
            canonical
                .split_once('/')
                .map(|(_, s)| s)
                .unwrap_or(canonical)
        )
    } else {
        canonical.into()
    };
    if thinking {
        format!("{name}-thinking")
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
        }
        headers
    }
    #[test]
    fn lan_clients_require_known_host_and_browser_origins_are_independent() {
        let policy = Policy::new(
            &ApiConfig {
                host: "0.0.0.0".into(),
                allowed_origins: vec!["http://192.168.1.20:3000".into()],
                ..Default::default()
            },
            &["192.168.1.10".into()],
        );
        assert!(policy.check(&headers("192.168.1.10:8321", None)).is_ok());
        assert!(
            policy
                .check(&headers("attacker.example:8321", None))
                .is_err()
        );
        assert!(
            policy
                .check(&headers(
                    "192.168.1.10:8321",
                    Some("http://192.168.1.20:3000")
                ))
                .is_ok()
        );
        assert!(
            policy
                .check(&headers(
                    "192.168.1.10:8321",
                    Some("https://attacker.example")
                ))
                .is_err()
        );
        assert!(
            policy
                .check(&headers("localhost:8321", Some("http://localhost:5173")))
                .is_ok()
        );
    }
    #[test]
    fn explicit_engine_and_thinking_survive_alias_translation() {
        let (alias, engine, thinking) = parse_model("mlx/my-qwen-thinking");
        assert_eq!(alias, "my-qwen");
        assert_eq!(
            rewrite_model("locals/canonical", engine, thinking),
            "mlx/canonical-thinking"
        );
    }
}
