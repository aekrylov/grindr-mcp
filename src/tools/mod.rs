//! MCP tool implementations, split into two groups:
//!
//! - [`generic`] — API discovery and the generic request tool. These don't
//!   target any specific endpoint.
//! - [`endpoints`] — convenience tools that each wrap a specific Grindr
//!   endpoint (auth, messaging, location, grid, profiles).
//!
//! Each group adds its tools to [`crate::GrindrServer`] through its own
//! `#[tool_router]` block (`generic_router` and `endpoint_router`); the two
//! routers are merged in `GrindrServer::new`.

pub mod endpoints;
pub mod generic;
