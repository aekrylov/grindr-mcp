//! A Model Context Protocol (MCP) server for the unofficial Grindr API.
//!
//! It exposes the [`grindr`](https://git.opengrind.org/open-grind/grindr.rs)
//! transport as MCP tools over stdio: log in, make authenticated requests to any
//! endpoint, and discover the API surface from the bundled OpenAPI document.
//!
//! The Grindr API sits behind Cloudflare and fingerprints clients by their TLS
//! and HTTP/2 handshake, so requests go through `grindr.rs`, which emulates the
//! Android app's fingerprint, header order and device identity.
//!
//! **For educational purposes only.** This is unofficial software, provided
//! solely for education and research into API interoperability. It is not
//! affiliated with or endorsed by Grindr. Automating access may violate
//! Grindr's Terms of Service; you are solely responsible for how you use it.

mod geohash;
mod openapi;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use grindr::{DeviceInfo, GrindrClient, Method, RawResponse, Session};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::stdio;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::{json, Value};

// ─── Tool argument types ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LoginArgs {
    /// Account email address.
    email: String,
    /// Account password.
    password: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RequestArgs {
    /// HTTP method, e.g. "GET", "POST", "PUT", "DELETE".
    method: String,
    /// Absolute API path beginning with '/', e.g. "/v3/me/profile" or
    /// "/v4/cascade". Find paths with grindr_list_endpoints.
    path: String,
    /// Optional JSON request body. Omit for GET/DELETE requests that take none.
    #[serde(default)]
    body: Option<Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListEndpointsArgs {
    /// Optional exact tag name to filter by (see grindr_list_tags), e.g.
    /// "messaging/conversations".
    #[serde(default)]
    tag: Option<String>,
    /// Optional case-insensitive substring to search path/summary/description.
    #[serde(default)]
    search: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DescribeEndpointArgs {
    /// The exact OpenAPI path, e.g. "/v3/rightnow/profiles/{profileId}".
    path: String,
    /// Optional HTTP method to narrow to a single operation on that path.
    #[serde(default)]
    method: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListConversationsArgs {
    /// 1-based page number for pagination. Defaults to 1. Use the `nextPage`
    /// from a previous response to page forward.
    #[serde(default)]
    page: Option<u32>,
    /// Only return conversations with unread messages.
    #[serde(default)]
    unread_only: Option<bool>,
    /// Only return conversations with favorited profiles.
    #[serde(default)]
    favorites_only: Option<bool>,
    /// Only return conversations whose participant is online now.
    #[serde(default)]
    online_now_only: Option<bool>,
    /// Only return conversations whose participant is "Right Now" active.
    #[serde(default)]
    right_now_only: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetMessagesArgs {
    /// Conversation id: two profile ids joined by ':' (smaller id first), e.g.
    /// "12345678:23456789". Get these from a conversation listing (POST
    /// /v3/inbox or /v4/inbox).
    conversation_id: String,
    /// Optional pagination cursor: return messages with ids *before* this value
    /// (i.e. older messages). Use the `pageKey` from a previous response to page
    /// back through history.
    #[serde(default)]
    page_key: Option<String>,
    /// Optional: include the other participant's profile in the response.
    #[serde(default)]
    include_profile: Option<bool>,
}

/// A location, given either as a ready-made 12-char geohash or as
/// latitude/longitude (which is encoded to a geohash for you).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct LocationArgs {
    /// A 12-character geohash, e.g. "ezjmgyern222" (Madrid). Takes precedence
    /// over latitude/longitude.
    #[serde(default)]
    geohash: Option<String>,
    /// Latitude in degrees (use together with longitude).
    #[serde(default)]
    latitude: Option<f64>,
    /// Longitude in degrees (use together with latitude).
    #[serde(default)]
    longitude: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateLocationArgs {
    #[serde(flatten)]
    location: LocationArgs,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BrowseGridArgs {
    /// Where to browse. If omitted, the pinned location (the last one set via
    /// grindr_update_location) is used.
    #[serde(flatten)]
    location: LocationArgs,
    /// 1-based page number.
    #[serde(default)]
    page: Option<i64>,
    /// Only show profiles that have a public photo.
    #[serde(default)]
    photo_only: Option<bool>,
    /// Only show profiles whose primary photo shows a face.
    #[serde(default)]
    face_only: Option<bool>,
    /// Only show profiles online now.
    #[serde(default)]
    online_only: Option<bool>,
    /// Only show recently active / fresh profiles.
    #[serde(default)]
    fresh: Option<bool>,
    /// Minimum age filter.
    #[serde(default)]
    age_min: Option<i64>,
    /// Maximum age filter.
    #[serde(default)]
    age_max: Option<i64>,
    /// Any additional /v4/cascade query parameters (e.g. "genders", "tribes",
    /// "lookingFor", "rightNow") as string values. See grindr_describe_endpoint
    /// for /v4/cascade.
    #[serde(default)]
    filters: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetProfileArgs {
    /// Numeric profile id of the person to view, e.g. "610887944".
    profile_id: String,
}

// ─── Server ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct GrindrServer {
    client: Arc<GrindrClient>,
    device: DeviceInfo,
    state_path: PathBuf,
    // Read by the code generated by #[tool_handler]; the lint can't see that.
    #[allow(dead_code)]
    tool_router: ToolRouter<GrindrServer>,
}

/// Render any serializable value as a single pretty-printed JSON text block.
fn json_result(value: Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(&value)
        .unwrap_or_else(|e| format!("<failed to serialize result: {e}>"));
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

/// A Grindr conversation id is `<digits>:<digits>` (two profile ids). Validating
/// it keeps caller mistakes from being pasted straight into the request path.
fn is_valid_conversation_id(id: &str) -> bool {
    match id.split_once(':') {
        Some((a, b)) => {
            !a.is_empty()
                && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Build CDN picture links for a profile from its media hashes. Each photo gets
/// a full-size and thumbnail URL on the public image CDN.
fn photo_links(profile: &Value) -> Vec<Value> {
    const CDN: &str = "https://cdns.grindr.com/images/profile";
    let mut out = Vec::new();

    // The primary/listing image, when present, plus the ordered `medias` array.
    let mut hashes: Vec<String> = Vec::new();
    if let Some(h) = profile
        .get("profileImageMediaHash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        hashes.push(h.to_owned());
    }
    if let Some(medias) = profile.get("medias").and_then(|m| m.as_array()) {
        for m in medias {
            if let Some(h) = m.get("mediaHash").and_then(|v| v.as_str()) {
                if !hashes.iter().any(|existing| existing == h) {
                    hashes.push(h.to_owned());
                }
            }
        }
    }

    for h in hashes {
        out.push(json!({
            "mediaHash": h,
            "full": format!("{CDN}/1024x1024/{h}"),
            "thumb": format!("{CDN}/320x320/{h}"),
        }));
    }
    out
}

/// Percent-encode a query-parameter value (RFC 3986 unreserved chars pass
/// through; everything else is `%XX`). Avoids pulling in a urlencoding crate.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[tool_router]
impl GrindrServer {
    fn new(client: Arc<GrindrClient>, device: DeviceInfo, state_path: PathBuf) -> Self {
        Self {
            client,
            device,
            state_path,
            tool_router: Self::tool_router(),
        }
    }

    /// Read the current session out of the client's session watch channel.
    fn current_session(&self) -> Option<Session> {
        self.client.session_receiver().borrow().clone()
    }

    /// Make an authenticated request, returning the raw status and body.
    async fn raw_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<RawResponse, McpError> {
        self.client
            .request_authenticated_raw(method, path, body)
            .await
            .map_err(|e| McpError::internal_error(format!("request failed: {e}"), None))
    }

    /// Parse a raw response body as JSON, falling back to a text string.
    fn parse_body(resp: &RawResponse) -> Value {
        match serde_json::from_slice::<Value>(&resp.body) {
            Ok(v) => v,
            Err(_) => Value::String(String::from_utf8_lossy(&resp.body).into_owned()),
        }
    }

    /// Make an authenticated request and render it as a `{status, body}` result,
    /// surfacing the body as JSON when it parses and as text otherwise. Shared by
    /// `grindr_request` and the higher-level endpoint tools.
    async fn authenticated_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self.raw_request(method, path, body).await?;
        json_result(json!({
            "status": resp.status,
            "body": Self::parse_body(&resp),
        }))
    }

    /// Resolve a [`LocationArgs`] to a 12-char geohash: explicit geohash wins,
    /// then latitude/longitude, then the pinned location on disk.
    fn resolve_geohash(&self, loc: &LocationArgs) -> Result<String, McpError> {
        if let Some(gh) = &loc.geohash {
            if !geohash::is_valid(gh) {
                return Err(McpError::invalid_params(
                    format!(
                        "geohash must be exactly 12 chars from the geohash alphabet, got {gh:?}"
                    ),
                    None,
                ));
            }
            return Ok(gh.clone());
        }
        if let (Some(lat), Some(lon)) = (loc.latitude, loc.longitude) {
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return Err(McpError::invalid_params(
                    format!("latitude/longitude out of range: {lat}, {lon}"),
                    None,
                ));
            }
            return Ok(geohash::encode(lat, lon, 12));
        }
        state::load_pinned_geohash(&self.state_path).ok_or_else(|| {
            McpError::invalid_params(
                "no location given and none pinned: pass geohash or latitude+longitude, \
                 or set one with grindr_update_location"
                    .to_owned(),
                None,
            )
        })
    }

    #[tool(
        description = "Log in to Grindr with email and password. The session is \
        persisted to disk and refreshed automatically, so you normally only need \
        to call this once. Returns the authenticated profile id."
    )]
    async fn grindr_login(
        &self,
        Parameters(LoginArgs { email, password }): Parameters<LoginArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.client.login(&email, &password).await {
            Ok(res) => json_result(json!({
                "logged_in": true,
                "profile_id": res.profile_id,
                "email": email,
            })),
            Err(e) => Err(McpError::internal_error(format!("login failed: {e}"), None)),
        }
    }

    #[tool(
        description = "Log out: clear the current session and remove it from disk. \
        The device identity is kept."
    )]
    async fn grindr_logout(&self) -> Result<CallToolResult, McpError> {
        self.client.logout().await;
        json_result(json!({ "logged_in": false }))
    }

    #[tool(
        description = "Report whether a session is active, with the account email, \
        profile id and session expiry, plus a summary of the emulated device."
    )]
    async fn grindr_session_status(&self) -> Result<CallToolResult, McpError> {
        let session = self.current_session();
        let session_json = match session {
            Some(s) => json!({
                "logged_in": true,
                "email": s.email,
                "profile_id": s.profile_id,
                "expires_at": s.expires_at,
                "kind": format!("{:?}", s.kind),
            }),
            None => json!({ "logged_in": false }),
        };
        json_result(json!({
            "session": session_json,
            "device": {
                "model": self.device.device_model,
                "manufacturer": self.device.manufacturer,
                "os": self.device.os,
                "timezone": self.device.timezone,
            },
            "state_file": self.state_path.display().to_string(),
        }))
    }

    #[tool(
        description = "Make an authenticated request to any Grindr API endpoint and \
        return the HTTP status and response body (parsed as JSON when possible). \
        The session token, device headers and TLS fingerprint are handled for you. \
        Use grindr_list_endpoints / grindr_describe_endpoint to discover paths."
    )]
    async fn grindr_request(
        &self,
        Parameters(RequestArgs { method, path, body }): Parameters<RequestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let method = Method::from_bytes(method.to_uppercase().as_bytes())
            .map_err(|e| McpError::invalid_params(format!("invalid HTTP method: {e}"), None))?;
        self.authenticated_request(method, &path, body).await
    }

    #[tool(
        description = "List the inbox conversations (most recent first), with \
        optional filters and pagination. Each entry includes the conversationId, \
        the other participant, unreadCount and a preview of the last message. Pass \
        unread_only=true to get just unread threads. Returns the raw status and \
        body (entries + a nextPage cursor)."
    )]
    async fn grindr_list_conversations(
        &self,
        Parameters(ListConversationsArgs {
            page,
            unread_only,
            favorites_only,
            online_now_only,
            right_now_only,
        }): Parameters<ListConversationsArgs>,
    ) -> Result<CallToolResult, McpError> {
        // The endpoint requires all of these fields to be present.
        let body = json!({
            "unreadOnly": unread_only.unwrap_or(false),
            "favoritesOnly": favorites_only.unwrap_or(false),
            "onlineNowOnly": online_now_only.unwrap_or(false),
            "rightNowOnly": right_now_only.unwrap_or(false),
            "chemistryOnly": false,
            "distanceMeters": Value::Null,
            "positions": [],
        });
        let path = format!("/v4/inbox?page={}", page.unwrap_or(1));
        self.authenticated_request(Method::POST, &path, Some(body))
            .await
    }

    #[tool(
        description = "Fetch the messages in a conversation (newest page first), \
        with optional pagination to older messages. Reading messages does NOT mark \
        them as read. Returns the raw status and body (messages, the other \
        participant, and a pageKey cursor for older messages)."
    )]
    async fn grindr_get_messages(
        &self,
        Parameters(GetMessagesArgs {
            conversation_id,
            page_key,
            include_profile,
        }): Parameters<GetMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !is_valid_conversation_id(&conversation_id) {
            return Err(McpError::invalid_params(
                format!(
                    "conversation_id must be two numeric profile ids joined by ':' \
                     (e.g. \"12345678:23456789\"), got {conversation_id:?}"
                ),
                None,
            ));
        }

        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(key) = &page_key {
            query.push(("pageKey", key.clone()));
        }
        if include_profile.unwrap_or(false) {
            query.push(("profile", "true".to_owned()));
        }

        let mut path = format!("/v5/chat/conversation/{conversation_id}/message");
        if !query.is_empty() {
            let qs: Vec<String> = query
                .iter()
                .map(|(k, v)| format!("{k}={}", percent_encode(v)))
                .collect();
            path.push('?');
            path.push_str(&qs.join("&"));
        }

        self.authenticated_request(Method::GET, &path, None).await
    }

    #[tool(
        description = "Update the location your profile broadcasts (where you show \
        up in other users' grids). Pass a 12-char geohash or latitude+longitude. \
        This also becomes the 'pinned' location used by grindr_browse_grid. \
        WARNING: setting a location inside the United Kingdom can lock the account \
        until you submit age-verification documents."
    )]
    async fn grindr_update_location(
        &self,
        Parameters(UpdateLocationArgs { location }): Parameters<UpdateLocationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let geohash = self.resolve_geohash(&location)?;
        let resp = self
            .raw_request(
                Method::PUT,
                "/v4/location",
                Some(json!({ "geohash": geohash })),
            )
            .await?;

        let pinned = if (200..300).contains(&resp.status) {
            state::save_pinned_geohash(&self.state_path, &geohash).is_ok()
        } else {
            false
        };

        json_result(json!({
            "status": resp.status,
            "geohash": geohash,
            "pinned": pinned,
            "body": Self::parse_body(&resp),
        }))
    }

    #[tool(
        description = "Browse the grid (cascade) of nearby profiles, with filters. \
        Location comes from an explicit geohash or latitude+longitude, or falls \
        back to the pinned location (grindr_update_location). Common filters are \
        exposed directly; pass anything else via `filters`. Returns the raw status \
        and body (profile items + nextPage)."
    )]
    async fn grindr_browse_grid(
        &self,
        Parameters(BrowseGridArgs {
            location,
            page,
            photo_only,
            face_only,
            online_only,
            fresh,
            age_min,
            age_max,
            filters,
        }): Parameters<BrowseGridArgs>,
    ) -> Result<CallToolResult, McpError> {
        let geohash = self.resolve_geohash(&location)?;

        let mut query: Vec<(String, String)> = vec![("nearbyGeoHash".into(), geohash)];
        let mut push_bool = |k: &str, v: Option<bool>| {
            if let Some(v) = v {
                query.push((k.to_owned(), v.to_string()));
            }
        };
        push_bool("photoOnly", photo_only);
        push_bool("faceOnly", face_only);
        push_bool("onlineOnly", online_only);
        push_bool("fresh", fresh);
        if let Some(v) = page {
            query.push(("pageNumber".into(), v.to_string()));
        }
        if let Some(v) = age_min {
            query.push(("ageMin".into(), v.to_string()));
        }
        if let Some(v) = age_max {
            query.push(("ageMax".into(), v.to_string()));
        }
        for (k, v) in filters.unwrap_or_default() {
            let s = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            query.push((k, s));
        }

        let qs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
            .collect();
        let path = format!("/v4/cascade?{}", qs.join("&"));
        self.authenticated_request(Method::GET, &path, None).await
    }

    #[tool(description = "View a user's full profile by numeric id: about/bio, \
        display name, age, tribes, looking-for, profile tags, stats and socials. \
        Photo media hashes are turned into ready-to-open CDN picture links. \
        Returns the full profile plus a `photos` list of URLs.")]
    async fn grindr_get_profile(
        &self,
        Parameters(GetProfileArgs { profile_id }): Parameters<GetProfileArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !profile_id.bytes().all(|c| c.is_ascii_digit()) || profile_id.is_empty() {
            return Err(McpError::invalid_params(
                format!("profile_id must be a numeric id, got {profile_id:?}"),
                None,
            ));
        }

        let resp = self
            .raw_request(Method::GET, &format!("/v7/profiles/{profile_id}"), None)
            .await?;
        let body = Self::parse_body(&resp);

        // The endpoint wraps a single profile in `profiles: [ ... ]`. Pull it out
        // and attach usable photo URLs built from each media hash.
        let profile = body
            .get("profiles")
            .and_then(|p| p.as_array())
            .and_then(|a| a.first())
            .cloned();

        match profile {
            Some(profile) => {
                let photos = photo_links(&profile);
                json_result(json!({
                    "status": resp.status,
                    "photos": photos,
                    "profile": profile,
                }))
            }
            None => json_result(json!({ "status": resp.status, "body": body })),
        }
    }

    #[tool(
        description = "List Grindr API endpoints from the bundled OpenAPI spec, \
        optionally filtered by exact tag and/or a case-insensitive search string. \
        Returns method, path, summary, tags and flags for each operation."
    )]
    async fn grindr_list_endpoints(
        &self,
        Parameters(ListEndpointsArgs { tag, search }): Parameters<ListEndpointsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let endpoints = openapi::list_endpoints(tag.as_deref(), search.as_deref());
        json_result(json!({
            "count": endpoints.len(),
            "endpoints": endpoints,
        }))
    }

    #[tool(
        description = "Describe a single API path in detail: parameters, request \
        body and response schemas, with $refs resolved inline. Pass an optional \
        method to narrow to one operation."
    )]
    async fn grindr_describe_endpoint(
        &self,
        Parameters(DescribeEndpointArgs { path, method }): Parameters<DescribeEndpointArgs>,
    ) -> Result<CallToolResult, McpError> {
        match openapi::describe_endpoint(&path, method.as_deref()) {
            Some(v) => json_result(v),
            None => Err(McpError::invalid_params(
                format!("no operation found for path {path:?} (method {method:?})"),
                None,
            )),
        }
    }

    #[tool(
        description = "List the API tags (categories) with their descriptions, for \
        use as the 'tag' filter in grindr_list_endpoints."
    )]
    async fn grindr_list_tags(&self) -> Result<CallToolResult, McpError> {
        json_result(json!({ "tags": openapi::list_tags() }))
    }
}

#[tool_handler]
impl ServerHandler for GrindrServer {
    fn get_info(&self) -> ServerInfo {
        let info = openapi::api_info();
        let instructions = format!(
            "MCP server for the unofficial Grindr API (transport: grindr.rs, which \
            emulates the Android app's TLS/HTTP2 fingerprint).\n\n\
            Workflow:\n\
            1. Call grindr_login(email, password) once (or grindr_session_status to \
            check an existing persisted session).\n\
            2. Discover endpoints with grindr_list_tags, grindr_list_endpoints and \
            grindr_describe_endpoint.\n\
            3. Call grindr_request(method, path, body?) to hit any endpoint.\n\n\
            For educational purposes only; unofficial and not affiliated with \
            Grindr.\n\n\
            Bundled spec: {}",
            info
        );
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(instructions)
    }
}

// ─── Session persistence ───────────────────────────────────────────────────────

/// Watch the client's session and write every change (login, refresh, logout)
/// back to the state file so it survives restarts.
fn spawn_session_persistence(client: &GrindrClient, device: DeviceInfo, path: PathBuf) {
    let mut rx = client.session_receiver();
    tokio::spawn(async move {
        loop {
            let session = rx.borrow_and_update().clone();
            if let Err(e) = state::save(&path, &device, session.as_ref()) {
                tracing::warn!("failed to persist session: {e}");
            }
            if rx.changed().await.is_err() {
                break; // client dropped
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs MUST go to stderr; stdout carries the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "grindr_mcp=info,warn".into()),
        )
        .init();

    let path = state::state_path();
    let (persisted, fresh) = state::load_or_init(&path)?;
    if fresh {
        state::save(&path, &persisted.device, persisted.session.as_ref())?;
        tracing::info!("generated new device identity at {}", path.display());
    }

    let client = GrindrClient::new(persisted.device.clone(), persisted.session.clone())?;
    spawn_session_persistence(&client, persisted.device.clone(), path.clone());

    let server = GrindrServer::new(Arc::new(client), persisted.device, path);

    tracing::info!("grindr-mcp serving on stdio");
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
