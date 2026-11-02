use crate::{
    metrics::RequestMetrics,
    service::Studio,
    types::{ApiConfig, ModelAlias, ServerProfile, ServerStatus},
};
use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, HttpBody, to_bytes},
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
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    #[cfg(test)]
    pub(crate) fn abort_for_test(&self) {
        self.task.abort();
    }

    pub fn begin_shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    pub async fn shutdown(mut self) {
        self.begin_shutdown();
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
    any_host: bool,
    any_origin: bool,
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
            any_host: config.allowed_hosts.is_empty(),
            any_origin: config.allowed_origins.is_empty(),
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
        if hostname.is_empty() || host.contains('@') {
            return Err("Invalid Host header".into());
        }
        let suffix = host
            .strip_prefix(authority.host())
            .ok_or("Invalid Host header")?;
        if !suffix.is_empty()
            && !suffix.strip_prefix(':').is_some_and(|port| {
                !port.is_empty()
                    && port.bytes().all(|byte| byte.is_ascii_digit())
                    && port.parse::<u16>().is_ok()
            })
        {
            return Err("Invalid Host header".into());
        }
        if !self.any_host && !self.hosts.contains(&hostname) && !self.hosts.contains("*") {
            return Err("This Host is not allowed by Studio".into());
        }
        let Some(origin) = headers.get(header::ORIGIN) else {
            return Ok(None);
        };
        let origin = origin.to_str().map_err(|_| "Invalid Origin header")?;
        if !valid_serialized_origin(origin) {
            return Err("Invalid Origin header".into());
        }
        let allowed = self.any_origin
            || self.origins.contains(origin.trim_end_matches('/'))
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

fn valid_serialized_origin(origin: &str) -> bool {
    if origin == "null" {
        return true;
    }
    if origin.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return false;
    }
    let Some((_, authority)) = origin.split_once("://") else {
        return false;
    };
    // Origin is a scheme/host/optional-port tuple, not a URL with a path or credentials.
    if authority.is_empty() || authority.contains(['/', '\\', '?', '#', '@']) {
        return false;
    }
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path(), "" | "/")
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
    // On macOS, SO_REUSEADDR can allow wildcard and specific-address listeners on the same
    // port. A successful bind alone does not establish an unambiguous public API endpoint.
    crate::ports::ensure_public_port_available(config.port)
        .await
        .with_context(|| format!("Cannot bind Studio API to {address}"))?;
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
    let description = describe(&config, &addresses, true, None);
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
        description,
        stop: Some(stop),
        task,
    })
}

pub fn description(config: &ApiConfig, running: bool, error: Option<String>) -> Value {
    describe(config, &interface_addresses(), running, error)
}

fn describe(
    config: &ApiConfig,
    addresses: &[String],
    running: bool,
    error: Option<String>,
) -> Value {
    let visible: Vec<_> = if config.host == "0.0.0.0" {
        addresses
            .iter()
            .filter(|v| !v.contains(':'))
            .cloned()
            .collect()
    } else if config.host == "::" {
        addresses.to_vec()
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
    json!({"enabled":config.enabled,"running":running,"host":config.host,"port":config.port,"error":error,"urls":urls,"lan":config.host!="127.0.0.1" && config.host!="::1"})
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
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
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
                    "authorization, content-type, x-api-key, anthropic-version, x-locals-session-id",
                )
            }),
        );
        headers.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(
                "x-locals-profile-id, x-locals-model-id, x-locals-server-alias",
            ),
        );
    }
}

async fn proxy(State(state): State<GatewayState>, request: Request) -> Response {
    if state.studio.server_is_stopping() || !state.studio.config.read().await.api.enabled {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The Studio model server is stopped",
        );
    }
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
        if path == "/metrics" {
            if !state.studio.config.read().await.api.metrics_enabled {
                return error(StatusCode::NOT_FOUND, "Unknown inference endpoint");
            }
            let statuses = state.studio.supervisor.list().await;
            let models = match crate::models::list(&state.studio.store) {
                Ok(models) => models,
                Err(failure) => {
                    tracing::warn!(%failure, "Cannot read model names for Prometheus metrics");
                    Vec::new()
                }
            };
            return match state.studio.metrics.render(&statuses, &models) {
                Ok(text) => (
                    [
                        (
                            header::CONTENT_TYPE,
                            "text/plain; version=0.0.4; charset=utf-8",
                        ),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    text,
                )
                    .into_response(),
                Err(failure) => {
                    tracing::error!(%failure, "Cannot encode Prometheus metrics");
                    error(StatusCode::INTERNAL_SERVER_ERROR, "Cannot encode metrics")
                }
            };
        }
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
            let profiles = state
                .studio
                .store
                .list::<ServerProfile>("profiles")
                .unwrap_or_default();
            let aliases = match state.studio.aliases.list() {
                Ok(aliases) => aliases,
                Err(failure) => {
                    return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{failure:#}"));
                }
            };
            if path.starts_with("/api/") {
                let mut models:Vec<_>=available.iter().filter(|server| !aliases.iter().any(|alias| alias.name == server.alias)).map(|s|json!({"name":s.alias,"model":s.alias,"modified_at":s.started_at,"size":0,"digest":s.model_id,"details":{"format":"mlx","family":"locals"}})).collect();
                models.extend(public_alias_models(&servers, &profiles, &aliases).into_iter().map(|model| json!({
                    "name":model["id"],"model":model["id"],"modified_at":"","size":0,"digest":model["id"],"details":{"format":"mlx","family":"locals"}
                })));
                return axum::Json(json!({"models":models})).into_response();
            }
            let data = public_models(&servers, &profiles, &aliases);
            return axum::Json(json!({"object":"list","data":data,"has_more":false}))
                .into_response();
        }
        if let Some(alias) = path
            .strip_prefix("/v1/models/")
            .or_else(|| path.strip_prefix("/models/"))
        {
            let servers = state.studio.supervisor.list().await;
            let profiles = state
                .studio
                .store
                .list::<ServerProfile>("profiles")
                .unwrap_or_default();
            let aliases = match state.studio.aliases.list() {
                Ok(aliases) => aliases,
                Err(failure) => {
                    return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{failure:#}"));
                }
            };
            if let Some(model) = public_models(&servers, &profiles, &aliases)
                .into_iter()
                .find(|model| model["id"] == alias)
            {
                return axum::Json(model).into_response();
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
    let metrics = state
        .studio
        .config
        .read()
        .await
        .api
        .metrics_enabled
        .then(|| Arc::new(state.studio.metrics.begin(&path)));
    let response = forward(state, request, &path, metrics.as_ref()).await;
    let Some(metrics) = metrics else {
        return response;
    };
    metrics.http_status(response.status().as_u16());
    let (parts, body) = response.into_parts();
    let expected = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| body.size_hint().exact());
    if expected == Some(0) {
        metrics.finish();
    }
    // The guard lives in the body, including before its first poll. Dropping a
    // response while awaiting headers or consuming a stream finalizes it once.
    let stream = async_stream::stream! {
        let mut chunks = body.into_data_stream();
        let mut received = 0u64;
        while let Some(chunk) = chunks.next().await {
            match &chunk {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    // Hyper need not poll EOF after receiving Content-Length bytes.
                    // Finalize before yielding that final chunk, rather than treating
                    // a fully consumed JSON response as a dropped/cancelled stream.
                    if expected == Some(received) {
                        metrics.finish();
                    }
                }
                Err(_) => {
                    metrics.transport_error();
                    metrics.finish();
                }
            }
            yield chunk;
        }
        metrics.finish();
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

async fn forward(
    state: GatewayState,
    request: Request,
    path: &str,
    metrics: Option<&Arc<RequestMetrics>>,
) -> Response {
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
        || state.studio.supervisor.has_ready_alias(undecorated).await
    {
        alias = undecorated.to_owned();
        thinking = false;
    }
    let session = match session_header(&parts.headers) {
        Ok(session) => session,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let opaque_context = path == "/api/generate"
        && value.get("context").is_some_and(|context| {
            !context.is_null() && context.as_array().is_none_or(|tokens| !tokens.is_empty())
        });
    let pooled = match state
        .studio
        .aliases
        .acquire(
            &requested,
            session,
            &state.studio.supervisor,
            opaque_context,
        )
        .await
    {
        Ok(lease) => lease,
        Err(failure) => {
            let status = if failure
                .downcast_ref::<crate::aliases::OpaqueContext>()
                .is_some()
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            return error(status, format!("{failure:#}"));
        }
    };
    let (lease, canonical) = match pooled {
        Some(route) => route,
        None => {
            if state.studio.supervisor.has_ready_alias(undecorated).await {
                alias = undecorated.to_owned();
                thinking = false;
            }
            match state.studio.supervisor.acquire(&alias).await {
                Ok(lease) => {
                    let canonical = rewrite_model(&lease.backend_model, engine, thinking);
                    (lease, canonical)
                }
                Err(failure) => {
                    return error(StatusCode::SERVICE_UNAVAILABLE, format!("{failure:#}"));
                }
            }
        }
    };
    if let Some(metrics) = metrics {
        metrics.set_profile(&lease.profile_id);
    }
    let route = [
        ("x-locals-profile-id", lease.profile_id.clone()),
        ("x-locals-model-id", lease.activity_status.model_id.clone()),
        ("x-locals-server-alias", lease.activity_status.alias.clone()),
    ];
    let activity = matches!(
        path,
        "/v1/chat/completions"
            | "/chat/completions"
            | "/v1/completions"
            | "/completions"
            | "/v1/messages"
            | "/api/chat"
            | "/api/generate"
    )
    .then(|| state.studio.supervisor.begin_activity(&lease));
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
            if let Some(metrics) = metrics {
                metrics.transport_error();
            }
            let mut response = error(
                StatusCode::BAD_GATEWAY,
                format!("Locals worker unavailable: {failure}"),
            );
            route_headers(&mut response, &route);
            return response;
        }
    };
    let status = response.status();
    let expected = response.content_length();
    let headers = response.headers().clone();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let observer = crate::activity::ResponseObserver::new(
        if status.is_success() { activity } else { None },
        content_type,
    )
    .with_metrics(if status.is_success() {
        metrics.cloned()
    } else {
        None
    });
    let stream = async_stream::stream! {
        let _lease=lease;
        let mut observer=observer;
        let mut chunks=response.bytes_stream();
        let mut received = 0u64;
        while let Some(chunk)=chunks.next().await {
            if let Ok(bytes)=&chunk {
                observer.push(bytes);
                received = received.saturating_add(bytes.len() as u64);
                if expected == Some(received) {
                    observer.complete();
                }
            }
            yield chunk;
        }
        observer.complete();
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
    route_headers(&mut result, &route);
    result
}

fn route_headers(response: &mut Response, route: &[(&'static str, String)]) {
    for (name, value) in route {
        response.headers_mut().remove(*name);
        if let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert(*name, value);
        }
    }
}

fn session_header(headers: &HeaderMap) -> std::result::Result<Option<&str>, &'static str> {
    let values = headers.get_all("x-locals-session-id");
    if values.iter().count() > 1 {
        return Err("Send exactly one X-Locals-Session-Id header");
    }
    match values.iter().next() {
        Some(value) => match value.to_str() {
            Ok(value)
                if !value.is_empty()
                    && value.len() <= 256
                    && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) =>
            {
                Ok(Some(value))
            }
            _ => Err("X-Locals-Session-Id must contain 1 to 256 visible ASCII characters"),
        },
        None => Ok(None),
    }
}

// Expose only variants the current worker actually advertises. Exact profile
// aliases have the same precedence here as in POST routing, even while stopped.
fn public_models(
    servers: &[ServerStatus],
    profiles: &[ServerProfile],
    aliases: &[ModelAlias],
) -> Vec<Value> {
    let exact_aliases: HashSet<_> = profiles
        .iter()
        .map(|profile| profile.alias.as_str())
        .chain(
            servers
                .iter()
                .filter(|server| matches!(server.state.as_str(), "ready" | "running"))
                .map(|server| server.alias.as_str()),
        )
        .chain(aliases.iter().map(|alias| alias.name.as_str()))
        .collect();
    let mut models = Vec::new();
    let mut published = HashSet::new();
    for server in servers
        .iter()
        .filter(|server| matches!(server.state.as_str(), "ready" | "running"))
    {
        if aliases.iter().any(|alias| alias.name == server.alias) {
            continue;
        }
        let mut add = |id: String, backend: Option<&str>| {
            if published.insert(id.clone()) {
                let mut model = json!({"id":id,"object":"model","owned_by":"locals-studio","created":0,"type":"model","display_name":id});
                if let Some(context) =
                    backend.and_then(|backend| server.backend_model_contexts.get(backend))
                {
                    if let Some(limit) = context.context_length.filter(|limit| *limit > 0) {
                        model["context_length"] = json!(limit);
                    }
                    if let Some(limit) = context.max_model_len.filter(|limit| *limit > 0) {
                        model["max_model_len"] = json!(limit);
                    }
                }
                models.push(model);
            }
        };
        add(server.alias.clone(), server.backend_model.as_deref());
        let Some(canonical) = server.backend_model.as_deref() else {
            continue;
        };
        let thinking_alias = format!("{}-thinking", server.alias);
        let thinking_available = !exact_aliases.contains(thinking_alias.as_str());
        if thinking_available
            && server
                .backend_models
                .contains(&format!("{canonical}-thinking"))
        {
            add(
                thinking_alias.clone(),
                Some(&format!("{canonical}-thinking")),
            );
        }
        let name = canonical
            .split_once('/')
            .map(|(_, name)| name)
            .unwrap_or(canonical);
        for backend in &server.backend_models {
            if backend == &format!("mlx/{name}") {
                add(format!("mlx/{}", server.alias), Some(backend));
            } else if thinking_available && backend == &format!("mlx/{name}-thinking") {
                add(format!("mlx/{thinking_alias}"), Some(backend));
            }
        }
    }
    for model in public_alias_models(servers, profiles, aliases) {
        if published.insert(model["id"].as_str().unwrap().to_owned()) {
            models.push(model);
        }
    }
    models
}

fn public_alias_models(
    servers: &[ServerStatus],
    profiles: &[ServerProfile],
    aliases: &[ModelAlias],
) -> Vec<Value> {
    let exact_names: HashSet<_> = profiles
        .iter()
        .map(|profile| profile.alias.as_str())
        .chain(
            servers
                .iter()
                .filter(|server| matches!(server.state.as_str(), "ready" | "running"))
                .map(|server| server.alias.as_str()),
        )
        .chain(aliases.iter().map(|alias| alias.name.as_str()))
        .collect();
    let mut models = Vec::new();
    for alias in aliases {
        for (id, engine, thinking) in [
            (alias.name.clone(), None, false),
            (format!("{}-thinking", alias.name), None, true),
            (format!("mlx/{}", alias.name), Some("mlx"), false),
            (format!("mlx/{}-thinking", alias.name), Some("mlx"), true),
        ] {
            if thinking && exact_names.contains(format!("{}-thinking", alias.name).as_str()) {
                continue;
            }
            let eligible: Vec<_> = servers
                .iter()
                .filter(|server| {
                    matches!(server.state.as_str(), "ready" | "running")
                        && alias.profile_ids.contains(&server.profile_id)
                        && profiles.iter().any(|profile| {
                            profile.id == server.profile_id && profile.model_id == server.model_id
                        })
                })
                .filter_map(|server| {
                    crate::supervisor::advertised_backend(server, engine, thinking)
                        .map(|backend| (server, backend))
                })
                .collect();
            if eligible.is_empty() {
                continue;
            }
            let mut model = json!({"id":id,"object":"model","owned_by":"locals-studio","created":0,"type":"model","display_name":id});
            for field in ["context_length", "max_model_len"] {
                let limits: Option<Vec<_>> = eligible
                    .iter()
                    .map(|(server, backend)| {
                        let context = server.backend_model_contexts.get(backend)?;
                        (if field == "context_length" {
                            context.context_length
                        } else {
                            context.max_model_len
                        })
                        .filter(|limit| *limit > 0)
                    })
                    .collect();
                if let Some(limit) = limits.and_then(|limits| limits.into_iter().min()) {
                    model[field] = json!(limit);
                }
            }
            models.push(model);
        }
    }
    models
}

pub(crate) fn parse_model(requested: &str) -> (String, Option<&str>, bool) {
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

pub(crate) fn rewrite_model(canonical: &str, engine: Option<&str>, thinking: bool) -> String {
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
    #[test]
    fn model_alias_session_header_has_one_unambiguous_bounded_value() {
        let mut headers = HeaderMap::new();
        assert_eq!(session_header(&headers), Ok(None));
        headers.insert("x-locals-session-id", HeaderValue::from_static("chat-123"));
        assert_eq!(session_header(&headers), Ok(Some("chat-123")));
        headers.append("x-locals-session-id", HeaderValue::from_static("chat-456"));
        assert!(session_header(&headers).is_err());
        for value in ["".to_string(), " ".to_string(), "x".repeat(257)] {
            headers.insert(
                "x-locals-session-id",
                HeaderValue::from_str(&value).unwrap(),
            );
            assert!(session_header(&headers).is_err());
        }
    }
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
                allowed_hosts: vec!["approved.example".into()],
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
    fn empty_lists_allow_arbitrary_hosts_and_valid_browser_origins_independently() {
        let open = Policy::new(&ApiConfig::default(), &[]);
        for origin in [
            None,
            Some("https://client.example:8443"),
            Some("tauri://localhost"),
            Some("null"),
        ] {
            assert!(
                open.check(&headers("arbitrary.example:8321", origin))
                    .is_ok()
            );
        }
        assert!(
            open.check(&headers("[2001:db8::1]:8321", Some("http://[::1]:5173")))
                .is_ok()
        );
        let host_restricted = Policy::new(
            &ApiConfig {
                allowed_hosts: vec!["approved.example".into()],
                ..Default::default()
            },
            &[],
        );
        assert!(
            host_restricted
                .check(&headers(
                    "approved.example:8321",
                    Some("https://unlisted.example")
                ))
                .is_ok()
        );
        assert!(
            host_restricted
                .check(&headers("unlisted.example:8321", None))
                .is_err()
        );
        let origin_restricted = Policy::new(
            &ApiConfig {
                allowed_origins: vec!["https://approved.example".into()],
                ..Default::default()
            },
            &[],
        );
        assert!(
            origin_restricted
                .check(&headers(
                    "unlisted.example:8321",
                    Some("https://approved.example")
                ))
                .is_ok()
        );
        assert!(
            origin_restricted
                .check(&headers(
                    "unlisted.example:8321",
                    Some("https://unlisted.example")
                ))
                .is_err()
        );
        assert!(
            origin_restricted
                .check(&headers("unlisted.example:8321", None))
                .is_ok()
        );
        assert!(
            origin_restricted
                .check(&headers(
                    "unlisted.example:8321",
                    Some("http://localhost:5173")
                ))
                .is_ok()
        );
    }

    #[test]
    fn opaque_null_origin_requires_unrestricted_or_explicit_permission() {
        let restricted = Policy::new(
            &ApiConfig {
                allowed_origins: vec!["https://approved.example".into()],
                ..Default::default()
            },
            &[],
        );
        assert!(
            restricted
                .check(&headers("localhost:8321", Some("null")))
                .is_err()
        );
        for allowed in [vec![], vec!["*".to_owned()], vec!["null".to_owned()]] {
            let policy = Policy::new(
                &ApiConfig {
                    allowed_origins: allowed,
                    ..Default::default()
                },
                &[],
            );
            assert_eq!(
                policy
                    .check(&headers("localhost:8321", Some("null")))
                    .unwrap(),
                Some("null".into())
            );
        }
    }

    #[test]
    fn unrestricted_lists_still_reject_missing_or_malformed_headers() {
        for lists in [vec![], vec!["*".to_owned()]] {
            let policy = Policy::new(
                &ApiConfig {
                    allowed_hosts: lists.clone(),
                    allowed_origins: lists,
                    ..Default::default()
                },
                &[],
            );
            assert!(policy.check(&HeaderMap::new()).is_err());
            for host in [
                "bad host:8321",
                ":8321",
                "user@localhost:8321",
                "localhost:wrong",
                "localhost:65536",
                "localhost:",
                "[::1]invalid",
            ] {
                assert!(policy.check(&headers(host, None)).is_err(), "Host {host}");
            }
            for origin in [
                "",
                "Null",
                "not-an-origin",
                "https:client.example",
                "https://client.example/",
                "https://client.example/path",
                "https://user:secret@client.example",
                "https://client.example?query",
                "https://client.example#fragment",
                "https://client.example https://other.example",
                "https://client.example:wrong",
            ] {
                assert!(
                    policy
                        .check(&headers("localhost:8321", Some(origin)))
                        .is_err(),
                    "Origin {origin}"
                );
            }
            let mut invalid_bytes = headers("localhost:8321", None);
            invalid_bytes.insert(header::ORIGIN, HeaderValue::from_bytes(&[0xff]).unwrap());
            assert!(policy.check(&invalid_bytes).is_err());
        }
    }

    #[test]
    fn wildcard_lists_keep_any_host_and_reflect_exact_origin_in_cors() {
        let policy = Policy::new(
            &ApiConfig {
                allowed_hosts: vec!["*".into()],
                allowed_origins: vec!["*".into()],
                ..Default::default()
            },
            &[],
        );
        for origin in ["https://arbitrary.example:8443", "null"] {
            let approved = policy
                .check(&headers("arbitrary.example:8321", Some(origin)))
                .unwrap();
            let mut response = StatusCode::NO_CONTENT.into_response();
            let requested = HeaderValue::from_static("content-type, authorization");
            cors(&mut response, approved.as_deref(), Some(&requested));
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                origin
            );
            assert_eq!(response.headers()[header::VARY], "Origin");
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
                "content-type, authorization"
            );
        }
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
