//! OAuth 2.0 for HTTP MCP servers, following the MCP authorization spec:
//! `WWW-Authenticate: Bearer` parsing, protected-resource metadata discovery
//! (RFC 9728), authorization-server metadata discovery (RFC 8414 with the
//! OpenID fallback), PKCE S256 (RFC 7636), code exchange, refresh, and a
//! JSON token store keyed by server name.
//!
//! The interactive browser step is left to the embedder: build the URL with
//! [`authorization_url`], deliver the code to [`exchange_code`], and save the
//! result with [`TokenStore::save`]. The HTTP transport then attaches the
//! bearer token and refreshes it once on `401`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `oauth` block of an MCP server config entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConfig {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Space-separated scopes to request.
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Authorization server issuer; skips RFC 9728 discovery when set.
    #[serde(default)]
    pub authorization_server: Option<String>,
    /// Token store file; defaults to `~/.omp/agent/mcp-oauth.json`.
    #[serde(default)]
    pub token_store: Option<String>,
}

/// Errors from discovery, token endpoints and the token store.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("OAuth HTTP error: {0}")]
    Http(String),
    #[error("OAuth endpoint returned {status}: {body}")]
    Endpoint { status: u16, body: String },
    #[error("OAuth metadata invalid: {0}")]
    Metadata(String),
    #[error("OAuth token store error: {0}")]
    Store(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// WWW-Authenticate
// ---------------------------------------------------------------------------

/// One challenge from a `WWW-Authenticate` header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Challenge {
    /// Auth scheme, lowercased (`bearer`, `basic`, ...).
    pub scheme: String,
    /// Auth parameters with lowercased names.
    pub params: BTreeMap<String, String>,
}

impl Challenge {
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    /// RFC 9728 `resource_metadata` URL.
    #[must_use]
    pub fn resource_metadata(&self) -> Option<&str> {
        self.param("resource_metadata")
    }
}

/// Parse a `WWW-Authenticate` header value (RFC 9110 §11.6.1) into its
/// challenges. Quoted strings may contain commas and `\"` escapes; token68
/// values are kept under the `token68` parameter.
#[must_use]
pub fn parse_www_authenticate(header: &str) -> Vec<Challenge> {
    let chars: Vec<char> = header.chars().collect();
    let mut i = 0;
    let mut out: Vec<Challenge> = Vec::new();
    let skip_ws = |i: &mut usize| {
        while *i < chars.len() && (chars[*i].is_whitespace() || chars[*i] == ',') {
            *i += 1;
        }
    };
    let read_token = |i: &mut usize| {
        let start = *i;
        while *i < chars.len() && !chars[*i].is_whitespace() && !matches!(chars[*i], ',' | '=') {
            *i += 1;
        }
        chars[start..*i].iter().collect::<String>()
    };
    loop {
        skip_ws(&mut i);
        if i >= chars.len() {
            break;
        }
        let scheme = read_token(&mut i);
        let mut challenge = Challenge {
            scheme: scheme.to_ascii_lowercase(),
            params: BTreeMap::new(),
        };
        loop {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let save = i;
            if i < chars.len() && chars[i] == ',' {
                i += 1;
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
            }
            if i >= chars.len() {
                break;
            }
            let name_start = i;
            let name = read_token(&mut i);
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if name.is_empty() || i >= chars.len() || chars[i] != '=' {
                if challenge.params.is_empty() && save == name_start && !name.is_empty() {
                    // token68 directly after the scheme.
                    challenge.params.insert("token68".to_string(), name);
                    continue;
                }
                // Next challenge starts here.
                i = name_start;
                break;
            }
            let eq_start = i;
            while i < chars.len() && chars[i] == '=' {
                i += 1;
            }
            // `abc==` right after the scheme is a padded token68, not a param.
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if challenge.params.is_empty()
                && save == name_start
                && (j >= chars.len() || chars[j] == ',')
            {
                let padding: String = chars[eq_start..i].iter().collect();
                challenge
                    .params
                    .insert("token68".to_string(), format!("{name}{padding}"));
                continue;
            }
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let value = if i < chars.len() && chars[i] == '"' {
                i += 1;
                let mut value = String::new();
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    value.push(chars[i]);
                    i += 1;
                }
                i += 1;
                value
            } else {
                let start = i;
                while i < chars.len() && chars[i] != ',' && !chars[i].is_whitespace() {
                    i += 1;
                }
                chars[start..i].iter().collect()
            };
            challenge.params.insert(name.to_ascii_lowercase(), value);
        }
        out.push(challenge);
    }
    out
}

/// The first `Bearer` challenge of a header, if any.
#[must_use]
pub fn bearer_challenge(header: &str) -> Option<Challenge> {
    parse_www_authenticate(header)
        .into_iter()
        .find(|c| c.scheme == "bearer")
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// PKCE verifier/challenge pair (S256).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub const METHOD: &'static str = "S256";

    /// Fresh pair from 32 random bytes (43-char verifier).
    pub fn generate() -> io::Result<Self> {
        Ok(Self::from_verifier(&random_url_token(32)?))
    }

    /// Pair for a known verifier.
    #[must_use]
    pub fn from_verifier(verifier: &str) -> Self {
        Self {
            verifier: verifier.to_string(),
            challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        }
    }
}

/// Random base64url string from `bytes` bytes of OS entropy (for `state`).
pub fn random_url_token(bytes: usize) -> io::Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// RFC 9728 protected resource metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProtectedResourceMetadata {
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// RFC 8414 authorization server metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    #[serde(default)]
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_challenge_methods_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
}

fn parse_url(url: &str) -> Result<Url, OAuthError> {
    Url::parse(url).map_err(|e| OAuthError::Metadata(format!("bad URL {url}: {e}")))
}

fn origin_with_path(url: &Url, path: &str) -> String {
    let mut out = format!("{}://", url.scheme());
    out.push_str(url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(path);
    out
}

/// RFC 9728 §3.1 well-known URLs for a resource: the path-suffixed form
/// first, then the root form.
pub fn protected_resource_metadata_urls(resource_url: &str) -> Result<Vec<String>, OAuthError> {
    let url = parse_url(resource_url)?;
    let path = url.path().trim_end_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() {
        urls.push(origin_with_path(
            &url,
            &format!("/.well-known/oauth-protected-resource{path}"),
        ));
    }
    urls.push(origin_with_path(
        &url,
        "/.well-known/oauth-protected-resource",
    ));
    Ok(urls)
}

/// RFC 8414 §3.1 metadata URLs for an issuer, with the OpenID Connect
/// Discovery fallbacks the MCP spec requires.
pub fn authorization_server_metadata_urls(issuer: &str) -> Result<Vec<String>, OAuthError> {
    let url = parse_url(issuer)?;
    let path = url.path().trim_end_matches('/');
    Ok(if path.is_empty() {
        vec![
            origin_with_path(&url, "/.well-known/oauth-authorization-server"),
            origin_with_path(&url, "/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            origin_with_path(
                &url,
                &format!("/.well-known/oauth-authorization-server{path}"),
            ),
            origin_with_path(&url, &format!("/.well-known/openid-configuration{path}")),
            origin_with_path(&url, &format!("{path}/.well-known/openid-configuration")),
        ]
    })
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<T>, OAuthError> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let text = response
        .text()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| OAuthError::Metadata(format!("{url}: {e}")))
}

/// Fetch protected resource metadata, preferring the URL announced in the
/// 401 challenge over the well-known locations.
pub async fn discover_protected_resource(
    client: &reqwest::Client,
    resource_url: &str,
    challenge: Option<&Challenge>,
) -> Result<ProtectedResourceMetadata, OAuthError> {
    let mut candidates = Vec::new();
    if let Some(url) = challenge.and_then(Challenge::resource_metadata) {
        candidates.push(url.to_string());
    }
    candidates.extend(protected_resource_metadata_urls(resource_url)?);
    for url in candidates {
        if let Some(meta) = get_json::<ProtectedResourceMetadata>(client, &url).await? {
            return Ok(meta);
        }
    }
    Err(OAuthError::Metadata(format!(
        "no protected resource metadata for {resource_url}"
    )))
}

/// Fetch authorization server metadata for `issuer`.
pub async fn discover_authorization_server(
    client: &reqwest::Client,
    issuer: &str,
) -> Result<AuthorizationServerMetadata, OAuthError> {
    for url in authorization_server_metadata_urls(issuer)? {
        if let Some(meta) = get_json::<AuthorizationServerMetadata>(client, &url).await? {
            return Ok(meta);
        }
    }
    Err(OAuthError::Metadata(format!(
        "no authorization server metadata for {issuer}"
    )))
}

/// Full discovery: configured issuer, else the first authorization server
/// of the resource metadata, else the resource origin itself (the MCP
/// 2025-03-26 fallback).
pub async fn discover(
    client: &reqwest::Client,
    resource_url: &str,
    config: &OAuthConfig,
    challenge: Option<&Challenge>,
) -> Result<AuthorizationServerMetadata, OAuthError> {
    let issuer = match &config.authorization_server {
        Some(issuer) => issuer.clone(),
        None => match discover_protected_resource(client, resource_url, challenge).await {
            Ok(meta) if !meta.authorization_servers.is_empty() => {
                meta.authorization_servers[0].clone()
            }
            _ => origin_with_path(&parse_url(resource_url)?, ""),
        },
    };
    discover_authorization_server(client, &issuer).await
}

/// Authorization request URL for the code + PKCE flow, with the RFC 8707
/// `resource` indicator.
pub fn authorization_url(
    metadata: &AuthorizationServerMetadata,
    config: &OAuthConfig,
    pkce: &Pkce,
    state: &str,
    resource: &str,
) -> Result<String, OAuthError> {
    let mut url = parse_url(&metadata.authorization_endpoint)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        if let Some(id) = &config.client_id {
            q.append_pair("client_id", id);
        }
        if let Some(uri) = &config.redirect_uri {
            q.append_pair("redirect_uri", uri);
        }
        if let Some(scope) = &config.scope {
            q.append_pair("scope", scope);
        }
        q.append_pair("state", state);
        q.append_pair("code_challenge", &pkce.challenge);
        q.append_pair("code_challenge_method", Pkce::METHOD);
        q.append_pair("resource", resource);
    }
    Ok(url.into())
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// Tokens issued by the authorization server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Absolute expiry, ms since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Refresh this long before the stated expiry.
pub const EXPIRY_SKEW_MS: u64 = 30_000;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl TokenSet {
    #[must_use]
    pub fn is_expired_at(&self, now: u64) -> bool {
        self.expires_at_ms
            .is_some_and(|at| now.saturating_add(EXPIRY_SKEW_MS) >= at)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

async fn token_request(
    client: &reqwest::Client,
    token_endpoint: &str,
    config: &OAuthConfig,
    mut form: Vec<(&str, String)>,
) -> Result<TokenSet, OAuthError> {
    if let Some(id) = &config.client_id {
        form.push(("client_id", id.clone()));
    }
    if let Some(secret) = &config.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let body = {
        let mut url = Url::parse("http://form.invalid/").expect("static URL");
        url.query_pairs_mut().extend_pairs(form.iter());
        url.query().unwrap_or_default().to_string()
    };
    let response = client
        .post(token_endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| OAuthError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(OAuthError::Endpoint {
            status: status.as_u16(),
            body: text,
        });
    }
    let parsed: TokenResponse = serde_json::from_str(&text)
        .map_err(|e| OAuthError::Metadata(format!("token response: {e}")))?;
    Ok(TokenSet {
        access_token: parsed.access_token,
        token_type: parsed.token_type,
        refresh_token: parsed.refresh_token,
        expires_at_ms: parsed
            .expires_in
            .map(|secs| now_ms().saturating_add(secs.saturating_mul(1000))),
        scope: parsed.scope,
    })
}

/// Exchange an authorization code (with its PKCE verifier) for tokens.
pub async fn exchange_code(
    client: &reqwest::Client,
    metadata: &AuthorizationServerMetadata,
    config: &OAuthConfig,
    code: &str,
    verifier: &str,
    resource: &str,
) -> Result<TokenSet, OAuthError> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("code_verifier", verifier.to_string()),
        ("resource", resource.to_string()),
    ];
    if let Some(uri) = &config.redirect_uri {
        form.push(("redirect_uri", uri.clone()));
    }
    token_request(client, &metadata.token_endpoint, config, form).await
}

/// Refresh tokens. A response without a new refresh token keeps the old one.
pub async fn refresh_token(
    client: &reqwest::Client,
    token_endpoint: &str,
    config: &OAuthConfig,
    refresh: &str,
    resource: &str,
) -> Result<TokenSet, OAuthError> {
    let form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh.to_string()),
        ("resource", resource.to_string()),
    ];
    let mut tokens = token_request(client, token_endpoint, config, form).await?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh.to_string());
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Token store
// ---------------------------------------------------------------------------

/// What the store keeps per server: tokens plus what refresh needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCredentials {
    pub tokens: TokenSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// JSON file of credentials keyed by server name (mode 0600 on Unix).
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `config.token_store` or `~/.omp/agent/mcp-oauth.json`.
    #[must_use]
    pub fn for_config(config: &OAuthConfig) -> Option<Self> {
        if let Some(path) = &config.token_store {
            return Some(Self::new(path));
        }
        let agent_dir = crate::config::user_config_path()?.parent()?.to_path_buf();
        Some(Self::new(agent_dir.join("mcp-oauth.json")))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_all(&self) -> BTreeMap<String, StoredCredentials> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_all(&self, all: &BTreeMap<String, StoredCredentials>) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(all).map_err(io::Error::other)?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)
    }

    #[must_use]
    pub fn load(&self, server: &str) -> Option<StoredCredentials> {
        self.load_all().remove(server)
    }

    pub fn save(&self, server: &str, credentials: &StoredCredentials) -> io::Result<()> {
        let mut all = self.load_all();
        all.insert(server.to_string(), credentials.clone());
        self.save_all(&all)
    }

    pub fn remove(&self, server: &str) -> io::Result<()> {
        let mut all = self.load_all();
        if all.remove(server).is_some() {
            self.save_all(&all)?;
        }
        Ok(())
    }
}

/// Per-server OAuth state used by the HTTP transport.
#[derive(Debug, Clone)]
pub struct OAuthSession {
    pub server: String,
    pub resource: String,
    pub config: OAuthConfig,
    pub store: TokenStore,
}

impl OAuthSession {
    /// Stored, unexpired access token; refreshes first when it is expired
    /// and a refresh token is available.
    pub async fn access_token(&self, client: &reqwest::Client) -> Option<String> {
        let creds = self.store.load(&self.server)?;
        if creds.tokens.is_expired_at(now_ms()) {
            return self
                .refresh(client)
                .await
                .ok()
                .map(|tokens| tokens.access_token);
        }
        Some(creds.tokens.access_token)
    }

    /// Refresh with the stored refresh token and persist the result.
    pub async fn refresh(&self, client: &reqwest::Client) -> Result<TokenSet, OAuthError> {
        let creds = self
            .store
            .load(&self.server)
            .ok_or_else(|| OAuthError::Metadata("no stored credentials".to_string()))?;
        let refresh = creds
            .tokens
            .refresh_token
            .clone()
            .ok_or_else(|| OAuthError::Metadata("no refresh token".to_string()))?;
        let endpoint = match &creds.token_endpoint {
            Some(endpoint) => endpoint.clone(),
            None => {
                discover(client, &self.resource, &self.config, None)
                    .await?
                    .token_endpoint
            }
        };
        let mut config = self.config.clone();
        if config.client_id.is_none() {
            config.client_id.clone_from(&creds.client_id);
        }
        let tokens = refresh_token(client, &endpoint, &config, &refresh, &self.resource).await?;
        self.store.save(
            &self.server,
            &StoredCredentials {
                tokens: tokens.clone(),
                token_endpoint: Some(endpoint),
                client_id: config.client_id,
            },
        )?;
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{MockServer, Reply};

    #[test]
    fn parses_bearer_challenge_with_quoted_commas() {
        let header = r#"Bearer realm="mcp, inc", error="invalid_token", error_description="The access token \"x\" expired", resource_metadata="https://api.example.com/.well-known/oauth-protected-resource""#;
        let c = bearer_challenge(header).unwrap();
        assert_eq!(c.param("realm"), Some("mcp, inc"));
        assert_eq!(c.param("error"), Some("invalid_token"));
        assert_eq!(
            c.param("error_description"),
            Some(r#"The access token "x" expired"#)
        );
        assert_eq!(
            c.resource_metadata(),
            Some("https://api.example.com/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn parses_multiple_challenges_and_token68() {
        let header = r#"Basic realm="x", Bearer scope="read write", Negotiate abc123=="#;
        let all = parse_www_authenticate(header);
        let schemes: Vec<_> = all.iter().map(|c| c.scheme.as_str()).collect();
        assert_eq!(schemes, ["basic", "bearer", "negotiate"]);
        assert_eq!(all[1].param("scope"), Some("read write"));
        assert_eq!(all[2].param("token68"), Some("abc123=="));
        assert!(bearer_challenge("Basic realm=x").is_none());
        assert_eq!(bearer_challenge("Bearer").unwrap().params.len(), 0);
        assert_eq!(
            bearer_challenge("bearer ERROR=insufficient_scope")
                .unwrap()
                .param("error"),
            Some("insufficient_scope")
        );
    }

    #[test]
    fn pkce_matches_rfc7636_vector() {
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(p.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let fresh = Pkce::generate().unwrap();
        assert_eq!(fresh.verifier.len(), 43);
        assert_ne!(fresh.verifier, Pkce::generate().unwrap().verifier);
        assert_eq!(Pkce::from_verifier(&fresh.verifier), fresh);
    }

    #[test]
    fn well_known_urls() {
        assert_eq!(
            protected_resource_metadata_urls("https://h.example:8443/v1/mcp/").unwrap(),
            [
                "https://h.example:8443/.well-known/oauth-protected-resource/v1/mcp",
                "https://h.example:8443/.well-known/oauth-protected-resource",
            ]
        );
        assert_eq!(
            authorization_server_metadata_urls("https://auth.example").unwrap(),
            [
                "https://auth.example/.well-known/oauth-authorization-server",
                "https://auth.example/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            authorization_server_metadata_urls("https://auth.example/tenant1").unwrap(),
            [
                "https://auth.example/.well-known/oauth-authorization-server/tenant1",
                "https://auth.example/.well-known/openid-configuration/tenant1",
                "https://auth.example/tenant1/.well-known/openid-configuration",
            ]
        );
        assert!(protected_resource_metadata_urls("not a url").is_err());
    }

    #[test]
    fn authorization_url_carries_pkce_and_resource() {
        let meta = AuthorizationServerMetadata {
            authorization_endpoint: "https://auth.example/authorize?x=1".into(),
            token_endpoint: "https://auth.example/token".into(),
            ..Default::default()
        };
        let config = OAuthConfig {
            client_id: Some("cid".into()),
            redirect_uri: Some("http://127.0.0.1:9/cb".into()),
            scope: Some("read write".into()),
            ..Default::default()
        };
        let pkce = Pkce::from_verifier("v");
        let url =
            authorization_url(&meta, &config, &pkce, "st", "https://mcp.example/mcp").unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["x"], "1");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["scope"], "read write");
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], pkce.challenge);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["resource"], "https://mcp.example/mcp");
    }

    #[test]
    fn expiry_uses_skew() {
        let t = TokenSet {
            access_token: "a".into(),
            expires_at_ms: Some(100_000),
            ..Default::default()
        };
        assert!(!t.is_expired_at(100_000 - EXPIRY_SKEW_MS - 1));
        assert!(t.is_expired_at(100_000 - EXPIRY_SKEW_MS));
        assert!(!TokenSet::default().is_expired_at(u64::MAX));
    }

    #[test]
    fn token_store_round_trip_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("nested").join("oauth.json"));
        assert!(store.load("a").is_none());
        let creds = StoredCredentials {
            tokens: TokenSet {
                access_token: "tok".into(),
                refresh_token: Some("ref".into()),
                ..Default::default()
            },
            token_endpoint: Some("http://x/token".into()),
            client_id: Some("cid".into()),
        };
        store.save("a", &creds).unwrap();
        store.save("b", &creds).unwrap();
        assert_eq!(store.load("a"), Some(creds.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.remove("a").unwrap();
        assert!(store.load("a").is_none());
        assert_eq!(store.load("b"), Some(creds));
    }

    #[test]
    fn config_parses_camel_case() {
        let c: OAuthConfig = serde_json::from_value(serde_json::json!({
            "clientId": "cid", "scope": "s", "authorizationServer": "https://a", "tokenStore": "/tmp/x"
        }))
        .unwrap();
        assert_eq!(c.client_id.as_deref(), Some("cid"));
        assert_eq!(c.authorization_server.as_deref(), Some("https://a"));
        assert_eq!(c.token_store.as_deref(), Some("/tmp/x"));
    }

    #[tokio::test]
    async fn discovery_follows_challenge_then_issuer() {
        let server = MockServer::start(|req| {
            match req.path.as_str() {
                "/custom-prm" => Reply::json(
                    200,
                    serde_json::json!({ "resource": "x", "authorization_servers": [format!("{}/tenant", req.base)] }),
                ),
                // RFC 8414 path-suffixed form is missing: fall back to OIDC.
                "/.well-known/openid-configuration/tenant" => Reply::json(
                    200,
                    serde_json::json!({
                        "issuer": format!("{}/tenant", req.base),
                        "authorization_endpoint": format!("{}/authorize", req.base),
                        "token_endpoint": format!("{}/token", req.base),
                        "code_challenge_methods_supported": ["S256"],
                    }),
                ),
                _ => Reply::status(404),
            }
        })
        .await;
        let challenge = bearer_challenge(&format!(
            r#"Bearer resource_metadata="{}/custom-prm""#,
            server.base
        ))
        .unwrap();
        let meta = discover(
            &reqwest::Client::new(),
            &format!("{}/mcp", server.base),
            &OAuthConfig::default(),
            Some(&challenge),
        )
        .await
        .unwrap();
        assert_eq!(meta.token_endpoint, format!("{}/token", server.base));
        assert_eq!(meta.code_challenge_methods_supported, ["S256"]);
        let paths = server.paths();
        assert_eq!(paths[0], "/custom-prm");
        assert!(paths.contains(&"/.well-known/oauth-authorization-server/tenant".to_string()));
    }

    #[tokio::test]
    async fn discovery_falls_back_to_resource_origin() {
        let server = MockServer::start(|req| match req.path.as_str() {
            "/.well-known/oauth-authorization-server" => Reply::json(
                200,
                serde_json::json!({
                    "authorization_endpoint": format!("{}/a", req.base),
                    "token_endpoint": format!("{}/t", req.base),
                }),
            ),
            _ => Reply::status(404),
        })
        .await;
        let meta = discover(
            &reqwest::Client::new(),
            &format!("{}/mcp", server.base),
            &OAuthConfig::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(meta.token_endpoint, format!("{}/t", server.base));
    }

    #[tokio::test]
    async fn exchange_and_refresh_post_forms() {
        let server = MockServer::start(|req| {
            let form: BTreeMap<String, String> =
                Url::parse(&format!("http://x/?{}", req.body))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect();
            match form.get("grant_type").map(String::as_str) {
                Some("authorization_code")
                    if form.get("code_verifier").map(String::as_str) == Some("ver")
                        && form["code"] == "c0de"
                        && form["client_id"] == "cid" =>
                {
                    Reply::json(
                        200,
                        serde_json::json!({ "access_token": "A1", "token_type": "Bearer", "refresh_token": "R1", "expires_in": 3600 }),
                    )
                }
                Some("refresh_token") if form["refresh_token"] == "R1" => {
                    Reply::json(200, serde_json::json!({ "access_token": "A2" }))
                }
                _ => Reply::json(400, serde_json::json!({ "error": "invalid_grant" })),
            }
        })
        .await;
        let client = reqwest::Client::new();
        let meta = AuthorizationServerMetadata {
            authorization_endpoint: format!("{}/a", server.base),
            token_endpoint: format!("{}/token", server.base),
            ..Default::default()
        };
        let config = OAuthConfig {
            client_id: Some("cid".into()),
            ..Default::default()
        };
        let tokens = exchange_code(&client, &meta, &config, "c0de", "ver", "r")
            .await
            .unwrap();
        assert_eq!(tokens.access_token, "A1");
        assert!(tokens.expires_at_ms.unwrap() > now_ms());
        let refreshed = refresh_token(&client, &meta.token_endpoint, &config, "R1", "r")
            .await
            .unwrap();
        assert_eq!(refreshed.access_token, "A2");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("R1"));
        let err = refresh_token(&client, &meta.token_endpoint, &config, "bad", "r")
            .await
            .unwrap_err();
        assert!(matches!(err, OAuthError::Endpoint { status: 400, .. }));
    }
}
