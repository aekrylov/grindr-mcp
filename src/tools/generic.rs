//! Generic API discovery and request tools.
//!
//! These are endpoint-agnostic: `grindr_request` calls an arbitrary path, and
//! the discovery tools read the bundled OpenAPI spec so an agent can find what
//! to call. Endpoint-specific convenience tools live in [`super::endpoints`].

use grindr::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{schemars, tool, tool_router, ErrorData as McpError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{json_result, openapi, GrindrServer};

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

#[tool_router(router = generic_router, vis = "pub(crate)")]
impl GrindrServer {
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
