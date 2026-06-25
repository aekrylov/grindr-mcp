//! Endpoint-specific convenience tools.
//!
//! Each tool here wraps a particular Grindr endpoint: auth/session, messaging,
//! location, grid browsing and profile viewing. The generic discovery/request
//! tools live in [`super::generic`].

use std::time::Duration;

use grindr::{Method, WsCommand, WsConnectionState};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{schemars, tool, tool_router, ErrorData as McpError};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::{geohash, json_result, state, GrindrServer};

/// How long to wait for the websocket to connect, and for a command ack.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WS_ACK_TIMEOUT: Duration = Duration::from_secs(12);

// ─── Tool argument types ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LoginArgs {
    /// Account email address.
    email: String,
    /// Account password.
    password: String,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendMessageArgs {
    /// Numeric profile id of the recipient, e.g. "877029791".
    profile_id: String,
    /// The text to send.
    text: String,
}

// ─── Helpers ────────────────────────────────────────────────────────────────────

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

#[tool_router(router = endpoint_router, vis = "pub(crate)")]
impl GrindrServer {
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

    /// Send a chat command over the realtime websocket and wait for the server's
    /// ack (the event echoing our `ref`). Chat is websocket-only in practice —
    /// the HTTP `/v4/chat/message/send` endpoint returns an internal error — so
    /// this mirrors what the Android app and the `grindr.rs` reference client do.
    async fn send_ws_command(&self, command_type: &str, payload: Value) -> Result<Value, McpError> {
        if self.current_session().is_none() {
            return Err(McpError::internal_error("not logged in", None));
        }

        // Open the websocket (idempotent) and wait until it's connected.
        self.client.connect().await;
        let mut state_rx = self.client.connection_state();
        if *state_rx.borrow() != WsConnectionState::Connected {
            let wait = async {
                loop {
                    if state_rx.changed().await.is_err() {
                        return false;
                    }
                    if *state_rx.borrow() == WsConnectionState::Connected {
                        return true;
                    }
                }
            };
            match timeout(WS_CONNECT_TIMEOUT, wait).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(McpError::internal_error(
                        "websocket closed before connecting",
                        None,
                    ))
                }
                Err(_) => {
                    return Err(McpError::internal_error(
                        "websocket did not connect within 15s",
                        None,
                    ))
                }
            }
        }

        // Subscribe before sending so we don't miss a fast ack.
        let mut events = self.client.ws_receiver();
        let ref_id = uuid::Uuid::new_v4().to_string();
        let command = WsCommand {
            r#type: command_type.to_owned(),
            ref_id: ref_id.clone(),
            payload,
        };

        self.client
            .ws_sender()
            .send(command)
            .await
            .map_err(|e| McpError::internal_error(format!("websocket send failed: {e}"), None))?;

        // Wait for the event whose `ref` matches our command.
        let wait_ack = async {
            loop {
                match events.recv().await {
                    Ok(ev) => {
                        if ev.payload.get("ref").and_then(|r| r.as_str()) == Some(ref_id.as_str()) {
                            return Some(ev.payload);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                }
            }
        };

        match timeout(WS_ACK_TIMEOUT, wait_ack).await {
            Ok(Some(payload)) => Ok(payload),
            Ok(None) => Err(McpError::internal_error(
                "websocket closed before the command was acked",
                None,
            )),
            // No ack doesn't necessarily mean failure — surface that honestly.
            Err(_) => Ok(json!({
                "acked": false,
                "note": "command dispatched but no ack within 12s; verify with grindr_get_messages",
            })),
        }
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

    #[tool(description = "Send a text chat message to a user over the realtime \
        websocket — the way the app does it. (The HTTP send endpoint returns an \
        internal error; chat is websocket-only in practice.) Opens the websocket \
        if needed and waits for the server ack. Be civil; do not use for spam.")]
    async fn grindr_send_message(
        &self,
        Parameters(SendMessageArgs { profile_id, text }): Parameters<SendMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        if profile_id.is_empty() || !profile_id.bytes().all(|c| c.is_ascii_digit()) {
            return Err(McpError::invalid_params(
                format!("profile_id must be a numeric id, got {profile_id:?}"),
                None,
            ));
        }
        if text.is_empty() {
            return Err(McpError::invalid_params("text must not be empty", None));
        }
        let target_id: i64 = profile_id
            .parse()
            .map_err(|_| McpError::invalid_params("profile_id is out of range", None))?;

        // Payload mirrors the HTTP send body (type / target / body).
        let payload = json!({
            "type": "Text",
            "target": { "type": "Direct", "targetId": target_id },
            "body": { "text": text },
        });
        let ack = self
            .send_ws_command("chat.v1.message.send", payload)
            .await?;

        json_result(json!({
            "sent": true,
            "recipient": profile_id,
            "ack": ack,
        }))
    }
}
