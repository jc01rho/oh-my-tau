//! Streamable-HTTP transport: JSON-RPC 2.0 over HTTP POST with SSE
//! responses, a port of `transports/http.ts` (also serves legacy `sse`
//! configs through the same POST+SSE surface, which is what omp's SSE
//! transport degrades to for requests).
//!
//! Carries the negotiated `MCP-Protocol-Version` header after `initialize`
//! and the server's `Mcp-Session-Id` on subsequent requests, sends
//! `Accept: application/json, text/event-stream`, parses both plain JSON and
//! `text/event-stream` bodies, and issues the spec's DELETE on close.
//!
//! With an `oauth` config block the stored bearer token is attached, and a
//! `401` triggers one token refresh + retry; an unrecoverable `401` surfaces
//! the raw `WWW-Authenticate` challenge in the error's `data`.

use std::sync::Arc;

use futures::StreamExt;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::config::McpServerConfig;
use crate::errors::{Failure, JsonRpcError, McpError, McpTransportError, TransportKind};
use crate::oauth::{bearer_challenge, OAuthSession, TokenStore};
use crate::transport::{McpTransport, NotificationCallback, PendingRequests, TransportFut};

/// One SSE event.
#[derive(Debug, Default)]
struct SseEvent {
    data: String,
}

/// Read `data:` lines from an SSE byte stream into JSON values, following
/// the standard `readSseEvents` shape (blank line terminates an event).
async fn read_sse_events(body: reqwest::Response) -> Vec<Result<Value, String>> {
    let mut events = Vec::new();
    let mut stream = body.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = find_newline(&buf) {
            let line_bytes = buf.drain(..=pos).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data:") {
                let event = SseEvent {
                    data: data.trim_start().to_string(),
                };
                if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
                    events.push(Ok(value));
                } else {
                    events.push(Err(format!("malformed SSE data: {}", event.data)));
                }
            }
        }
    }
    events
}

fn find_newline(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|&b| b == b'\n')
}

/// Mutable connection state shared with in-flight requests.
#[derive(Default)]
struct HttpState {
    session_id: Option<String>,
    protocol_version: Option<String>,
    next_id: u64,
}

/// HTTP(SSE) transport state.
pub struct HttpTransport {
    config: McpServerConfig,
    timeout_ms: u64,
    client: reqwest::Client,
    state: Arc<Mutex<HttpState>>,
    oauth: Option<Arc<OAuthSession>>,
    /// Server→client notification sink installed by the owner (`None`: dropped).
    on_notification: Option<NotificationCallback>,
}

impl HttpTransport {
    /// Build a transport for an `http` (Streamable) or legacy `sse` server.
    pub fn new(server_name: &str, config: McpServerConfig, _cwd: Arc<std::path::PathBuf>) -> Self {
        let timeout_ms = config.timeout_ms();
        let oauth = config.oauth().and_then(|oauth| {
            Some(Arc::new(OAuthSession {
                server: server_name.to_string(),
                resource: config.url.clone().unwrap_or_default(),
                store: TokenStore::for_config(&oauth)?,
                config: oauth,
            }))
        });
        Self {
            config,
            timeout_ms,
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(HttpState::default())),
            oauth,
            on_notification: None,
        }
    }

    /// Record the protocol version negotiated by `initialize`.
    fn set_protocol_version_sync(&self, version: &str) {
        if let Ok(mut guard) = self.state.try_lock() {
            guard.protocol_version = Some(version.to_string());
        }
    }

    fn headers(&self, state: &HttpState) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(session) = &state.session_id {
            map.insert(
                "mcp-session-id",
                HeaderValue::from_str(session).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
        if let Some(version) = &state.protocol_version {
            map.insert(
                "mcp-protocol-version",
                HeaderValue::from_str(version).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
        for (name, value) in self.config.headers.clone().unwrap_or_default() {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                if name != "mcp-protocol-version" {
                    map.insert(name, value);
                }
            }
        }
        map
    }
}

impl HttpTransport {
    fn http_error(status: reqwest::StatusCode, body_text: &str) -> McpError {
        McpError::Transport(McpTransportError {
            transport: TransportKind::Http,
            stage: Failure::Receive,
            failure: Failure::HttpStatus,
            retryable: matches!(status.as_u16(), 404 | 502 | 503),
            message: format!("HTTP {status}: {}", truncate(body_text, 512)),
            code: Some(status.to_string()),
            data: None,
        })
    }
}

fn set_bearer(headers: &mut HeaderMap, token: &str) {
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, value);
    }
}

/// Attach the stored OAuth token unless the config already sets
/// `Authorization` itself.
async fn attach_token(
    client: &reqwest::Client,
    oauth: Option<&OAuthSession>,
    headers: &mut HeaderMap,
) {
    if let Some(auth) = oauth {
        if !headers.contains_key(AUTHORIZATION) {
            if let Some(token) = auth.access_token(client).await {
                set_bearer(headers, &token);
            }
        }
    }
}

/// Error for a `401` that refresh could not fix; `data` keeps the raw
/// `WWW-Authenticate` header so callers can start discovery from it.
fn unauthorized_error(www_authenticate: Option<&str>, body_text: &str) -> McpError {
    let mut message = "HTTP 401 Unauthorized: authorization required".to_string();
    if let Some(challenge) = www_authenticate.and_then(bearer_challenge) {
        if let Some(error) = challenge.param("error") {
            message.push_str(&format!(" (error={error})"));
        }
        if let Some(url) = challenge.resource_metadata() {
            message.push_str(&format!("; resource_metadata={url}"));
        }
    }
    if !body_text.is_empty() {
        message.push_str(&format!(": {}", truncate(body_text, 512)));
    }
    McpError::Transport(McpTransportError {
        transport: TransportKind::Http,
        stage: Failure::Receive,
        failure: Failure::HttpStatus,
        retryable: false,
        message,
        code: Some(reqwest::StatusCode::UNAUTHORIZED.to_string()),
        data: www_authenticate.map(str::to_string),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..s.floor_char_boundary(max)])
    }
}

impl HttpTransport {}

/// Send one JSON-RPC request over HTTP and return the response `result`.
#[allow(clippy::too_many_arguments)]
async fn send_request(
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    id: Value,
    method: String,
    params: Value,
    timeout_ms: u64,
    state: Arc<Mutex<HttpState>>,
    oauth: Option<Arc<OAuthSession>>,
    notification: Option<NotificationCallback>,
) -> Result<Value, McpError> {
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let expected_key = PendingRequests::key(&id);
    let session_cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let session_cell_inner = session_cell.clone();
    let send = async move {
        let mut headers = headers;
        attach_token(&client, oauth.as_deref(), &mut headers).await;
        let mut refreshed = false;
        let response = loop {
            let response = client
                .post(&url)
                .headers(headers.clone())
                .json(&body)
                .send()
                .await
                .map_err(|err| {
                    let retryable = err.is_connect() || err.is_timeout();
                    McpError::Transport(McpTransportError {
                        transport: TransportKind::Http,
                        stage: Failure::Connect,
                        failure: Failure::Connect,
                        retryable,
                        message: format!("fetch failed: {err}"),
                        code: err.status().map(|s| s.to_string()),
                        data: None,
                    })
                })?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED {
                break response;
            }
            let www_authenticate = response
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            if !refreshed {
                if let Some(auth) = &oauth {
                    match auth.refresh(&client).await {
                        Ok(tokens) => {
                            set_bearer(&mut headers, &tokens.access_token);
                            refreshed = true;
                            continue;
                        }
                        Err(error) => tracing::debug!("MCP OAuth refresh failed: {error}"),
                    }
                }
            }
            let text = response.text().await.unwrap_or_default();
            return Err(unauthorized_error(www_authenticate.as_deref(), &text));
        };
        let status = response.status();
        let session: Option<String> = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if let Some(s) = &session {
            *session_cell_inner.lock().await = Some(s.clone());
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if content_type.contains("text/event-stream") {
            // Streamable HTTP SSE response: scan events for our response.
            let events = read_sse_events(response).await;
            for event in events {
                match event {
                    Ok(msg) => {
                        // A notification carried on the request's stream: the
                        // runtime handles it first (no HTTP-side handling yet),
                        // then it goes to the installed sink.
                        if msg.get("method").is_some() && msg.get("id").is_none() {
                            if let Some(on_notification) = &notification {
                                on_notification(
                                    msg.get("method")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default(),
                                    msg.get("params").cloned().unwrap_or(Value::Null),
                                );
                            }
                            continue;
                        }
                        if let Some(response) = extract_response(&msg, &expected_key) {
                            return response;
                        }
                    }
                    Err(err) => tracing::debug!("ignoring malformed SSE event: {err}"),
                }
            }
            return Err(McpError::Transport(McpTransportError {
                transport: TransportKind::Http,
                stage: Failure::Receive,
                failure: Failure::Eof,
                retryable: true,
                message: "SSE stream closed before a response arrived".to_string(),
                code: None,
                data: None,
            }));
        }
        let text = response.text().await.map_err(|err| {
            McpError::Transport(McpTransportError {
                transport: TransportKind::Http,
                stage: Failure::Receive,
                failure: Failure::Receive,
                retryable: true,
                message: format!("failed to read response body: {err}"),
                code: None,
                data: None,
            })
        })?;
        if !status.is_success() {
            return Err(HttpTransport::http_error(status, &text));
        }
        parse_json_rpc(&text)
    };
    let result = if crate::timeout::is_timeout_enabled(timeout_ms) {
        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), send).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                return Err(McpError::Transport(McpTransportError {
                    transport: TransportKind::Http,
                    stage: Failure::Timeout,
                    failure: Failure::Timeout,
                    retryable: false,
                    message: format!("Request timeout after {timeout_ms}ms"),
                    code: None,
                    data: None,
                }));
            }
        }
    } else {
        send.await
    };
    if result.is_ok() {
        if let Some(session) = session_cell.lock().await.take() {
            state.lock().await.session_id = Some(session);
        }
        let mut guard = state.lock().await;
        guard.next_id += 1;
    }
    result
}

impl Clone for HttpState {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            protocol_version: self.protocol_version.clone(),
            next_id: self.next_id,
        }
    }
}

impl McpTransport for HttpTransport {
    fn set_notification_handler(&mut self, handler: NotificationCallback) {
        self.on_notification = Some(handler);
    }

    fn set_protocol_version(&mut self, version: &str) {
        self.set_protocol_version_sync(version);
    }

    fn request(&mut self, method: &str, params: Value) -> TransportFut<Result<Value, McpError>> {
        let state = futures::executor::block_on(self.state.lock()).clone();
        let id = Value::from(state.next_id + 1);
        let url = self.config.url.clone().unwrap_or_default();
        let headers = self.headers(&state);
        let client = self.client.clone();
        let timeout_ms = self.timeout_ms;
        let state = self.state.clone();
        let method = method.to_string();
        let oauth = self.oauth.clone();
        let notification = self.on_notification.clone();
        Box::pin(send_request(
            client,
            url,
            headers,
            id,
            method,
            params,
            timeout_ms,
            state,
            oauth,
            notification,
        ))
    }

    fn notify(&mut self, method: &str, params: Value) -> TransportFut<Result<(), McpError>> {
        let url = self.config.url.clone().unwrap_or_default();
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let state = futures::executor::block_on(self.state.lock()).clone();
        let mut headers = self.headers(&state);
        let client = self.client.clone();
        let oauth = self.oauth.clone();
        Box::pin(async move {
            attach_token(&client, oauth.as_deref(), &mut headers).await;
            let request = client.post(&url).headers(headers).json(&body);
            let response = request.send().await.map_err(|err| {
                McpError::Transport(McpTransportError {
                    transport: TransportKind::Http,
                    stage: Failure::Connect,
                    failure: Failure::Connect,
                    retryable: true,
                    message: format!("fetch failed: {err}"),
                    code: err.status().map(|s| s.to_string()),
                    data: None,
                })
            })?;
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(Self::http_error(status, &text));
            }
            Ok(())
        })
    }

    fn close(&mut self) -> TransportFut<()> {
        let url = self.config.url.clone().unwrap_or_default();
        let state = self.state.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let session = state.lock().await.session_id.take();
            if let Some(session) = session {
                let _ = client
                    .delete(&url)
                    .header("mcp-session-id", session)
                    .send()
                    .await;
            }
        })
    }
}

/// Extract a JSON-RPC response destined for our request from one SSE event;
/// notification frames are handed to the transport's sink by the caller, and
/// server→client requests are ignored here (the streaming GET listener that
/// answers them is not ported).
fn extract_response(msg: &Value, expected_key: &str) -> Option<Result<Value, McpError>> {
    let obj = msg.as_object()?;
    if obj.contains_key("method") {
        return None;
    }
    let id = obj.get("id")?;
    if PendingRequests::key(id) != expected_key {
        return None;
    }
    if let Some(error) = obj.get("error") {
        return Some(Err(McpError::JsonRpc(
            serde_json::from_value::<JsonRpcError>(error.clone()).unwrap_or(JsonRpcError {
                code: 0,
                message: "malformed JSON-RPC error response".to_string(),
                data: None,
            }),
        )));
    }
    Some(Ok(obj.get("result").cloned().unwrap_or(Value::Null)))
}

/// Parse a plain JSON JSON-RPC response body.
fn parse_json_rpc(text: &str) -> Result<Value, McpError> {
    let value: Value = serde_json::from_str(text).map_err(|err| {
        McpError::Transport(McpTransportError {
            transport: TransportKind::Http,
            stage: Failure::Decode,
            failure: Failure::Decode,
            retryable: false,
            message: format!("malformed JSON-RPC response: {err}"),
            code: None,
            data: None,
        })
    })?;
    let Some(obj) = value.as_object() else {
        return Err(McpError::Config("malformed JSON-RPC response".to_string()));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::Config("malformed JSON-RPC response".to_string()));
    }
    if let Some(error) = obj.get("error") {
        return Err(McpError::JsonRpc(
            serde_json::from_value::<JsonRpcError>(error.clone()).unwrap_or(JsonRpcError {
                code: 0,
                message: "malformed JSON-RPC error response".to_string(),
                data: None,
            }),
        ));
    }
    Ok(obj.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::{StoredCredentials, TokenSet};
    use crate::test_http::{MockServer, Reply};
    use std::path::PathBuf;

    fn transport(url: &str, oauth: Option<Value>) -> HttpTransport {
        let mut cfg = json!({ "type": "http", "url": url });
        if let Some(oauth) = oauth {
            cfg["oauth"] = oauth;
        }
        HttpTransport::new(
            "srv",
            serde_json::from_value(cfg).unwrap(),
            Arc::new(PathBuf::from(".")),
        )
    }

    fn ok(req: &crate::test_http::Request) -> Reply {
        let id = req.json()["id"].clone();
        Reply::json(
            200,
            json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } }),
        )
    }

    #[tokio::test]
    async fn unauthorized_without_oauth_surfaces_challenge() {
        let server = MockServer::start(|req| {
            Reply::status(401).header(
                "www-authenticate",
                &format!(
                    r#"Bearer error="invalid_token", resource_metadata="{}/.well-known/oauth-protected-resource""#,
                    req.base
                ),
            )
        })
        .await;
        let mut t = transport(&format!("{}/mcp", server.base), None);
        let McpError::Transport(err) = t.request("tools/list", json!({})).await.unwrap_err() else {
            panic!("expected transport error");
        };
        assert_eq!(err.failure, Failure::HttpStatus);
        assert!(!err.retryable);
        assert_eq!(err.code.as_deref(), Some("401 Unauthorized"));
        assert!(err.message.contains("error=invalid_token"));
        let challenge = bearer_challenge(err.data.as_deref().unwrap()).unwrap();
        assert_eq!(
            challenge.resource_metadata(),
            Some(format!("{}/.well-known/oauth-protected-resource", server.base).as_str())
        );
        assert!(server.requests()[0].header("authorization").is_none());
    }

    #[tokio::test]
    async fn stored_token_is_sent_and_refreshed_on_401() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("oauth.json");
        let server = MockServer::start(|req| match req.path.as_str() {
            "/token" if req.body.contains("refresh_token=R1") => Reply::json(
                200,
                json!({ "access_token": "fresh", "refresh_token": "R2", "expires_in": 3600 }),
            ),
            "/mcp" if req.header("authorization") == Some("Bearer fresh") => ok(req),
            "/mcp" => {
                Reply::status(401).header("www-authenticate", r#"Bearer error="invalid_token""#)
            }
            _ => Reply::status(404),
        })
        .await;
        TokenStore::new(&store_path)
            .save(
                "srv",
                &StoredCredentials {
                    tokens: TokenSet {
                        access_token: "stale".into(),
                        refresh_token: Some("R1".into()),
                        ..Default::default()
                    },
                    token_endpoint: Some(format!("{}/token", server.base)),
                    client_id: Some("cid".into()),
                },
            )
            .unwrap();
        let mut t = transport(
            &format!("{}/mcp", server.base),
            Some(json!({ "tokenStore": store_path })),
        );
        let result = t.request("tools/list", json!({})).await.unwrap();
        assert_eq!(result, json!({ "ok": true }));
        let requests = server.requests();
        let mcp: Vec<_> = requests.iter().filter(|r| r.path == "/mcp").collect();
        assert_eq!(mcp.len(), 2);
        assert_eq!(mcp[0].header("authorization"), Some("Bearer stale"));
        assert_eq!(mcp[1].header("authorization"), Some("Bearer fresh"));
        let token_req = requests.iter().find(|r| r.path == "/token").unwrap();
        assert_eq!(token_req.method, "POST");
        assert!(token_req.body.contains("client_id=cid"));
        let saved = TokenStore::new(&store_path).load("srv").unwrap();
        assert_eq!(saved.tokens.access_token, "fresh");
        assert_eq!(saved.tokens.refresh_token.as_deref(), Some("R2"));
    }

    #[tokio::test]
    async fn refresh_retries_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("oauth.json");
        let server = MockServer::start(|req| match req.path.as_str() {
            "/token" => Reply::json(200, json!({ "access_token": "still-bad" })),
            _ => Reply::status(401),
        })
        .await;
        TokenStore::new(&store_path)
            .save(
                "srv",
                &StoredCredentials {
                    tokens: TokenSet {
                        access_token: "a".into(),
                        refresh_token: Some("r".into()),
                        ..Default::default()
                    },
                    token_endpoint: Some(format!("{}/token", server.base)),
                    client_id: None,
                },
            )
            .unwrap();
        let mut t = transport(
            &format!("{}/mcp", server.base),
            Some(json!({ "tokenStore": store_path })),
        );
        let err = t.request("ping", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("HTTP 401"));
        assert_eq!(server.paths(), ["/mcp", "/token", "/mcp"]);
    }

    #[tokio::test]
    async fn notify_attaches_bearer_and_explicit_header_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("oauth.json");
        TokenStore::new(&store_path)
            .save(
                "srv",
                &StoredCredentials {
                    tokens: TokenSet {
                        access_token: "tok".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        let server = MockServer::start(|_| Reply::status(202)).await;
        let url = format!("{}/mcp", server.base);
        let mut t = transport(&url, Some(json!({ "tokenStore": store_path })));
        t.notify("notifications/initialized", json!({}))
            .await
            .unwrap();
        let cfg = json!({
            "type": "http", "url": url,
            "headers": { "Authorization": "Bearer static" },
            "oauth": { "tokenStore": store_path },
        });
        let mut explicit = HttpTransport::new(
            "srv",
            serde_json::from_value(cfg).unwrap(),
            Arc::new(PathBuf::from(".")),
        );
        explicit.notify("x", json!({})).await.unwrap();
        let requests = server.requests();
        assert_eq!(requests[0].header("authorization"), Some("Bearer tok"));
        assert_eq!(requests[1].header("authorization"), Some("Bearer static"));
    }
}
