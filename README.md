# grindr-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server for the
**unofficial** Grindr API. It lets an MCP client (Claude Code, Claude Desktop,
etc.) log in to Grindr and call any endpoint, with API discovery built in from a
bundled OpenAPI spec.

It is built on [`grindr.rs`](https://git.opengrind.org/open-grind/grindr.rs),
which talks to Grindr the same way the Android app does — same TLS/HTTP2
fingerprint, header order and device identity — so requests get past
Cloudflare. The API surface comes from the
[Open Grind](https://opengrind.org) project's OpenAPI document.

> ⚠️ **For educational purposes only.** This project is provided solely for
> education and research into API interoperability. It is **unofficial** and not
> affiliated with, authorized, or endorsed by Grindr. Automating access may
> violate Grindr's Terms of Service. Use it only with your own account and at
> your own risk — you are solely responsible for how you use it.

## Tools

| Tool | Purpose |
| --- | --- |
| `grindr_login` | Log in with email + password. Session is persisted and auto-refreshed. |
| `grindr_logout` | Clear the session and remove it from disk. |
| `grindr_session_status` | Whether a session is active, plus device summary. |
| `grindr_request` | Make an authenticated request to any endpoint (`method`, `path`, optional JSON `body`). Returns status + body. |
| `grindr_update_location` | Update the location your profile broadcasts (`geohash`, or `latitude`+`longitude`). Also sets the pinned location used by the grid. |
| `grindr_browse_grid` | Browse nearby profiles with filters (`photo_only`, `online_only`, `age_min/max`, …); location from a geohash, lat/long, or the pinned location. |
| `grindr_get_profile` | View a user's full profile by `profile_id`: bio, age, tribes, tags, stats, socials — with photo media hashes turned into CDN picture links. |
| `grindr_list_conversations` | List inbox conversations (most recent first), with `unread_only` / `favorites_only` / `online_now_only` / `right_now_only` filters and `page`. |
| `grindr_get_messages` | Fetch the messages in a conversation (`conversation_id`, optional `page_key` for older messages). Does not mark as read. |
| `grindr_send_message` | Send a text message to a user (`profile_id`, `text`) over the realtime **websocket** — chat is websocket-only in practice; the HTTP send endpoint returns an internal error. |
| `grindr_list_endpoints` | List endpoints from the OpenAPI spec, filter by `tag` and/or `search`. |
| `grindr_describe_endpoint` | Full details for a path: parameters, request/response schemas (with `$ref`s inlined). |
| `grindr_list_tags` | List API tags (categories) for use as the `tag` filter. |

`grindr_update_location`, `grindr_browse_grid`, `grindr_get_profile`,
`grindr_list_conversations` and `grindr_get_messages` are convenience wrappers
over common endpoints; anything else is reachable through the generic
`grindr_request`.

> ⚠️ `grindr_update_location` changes where your profile appears to others.
> Setting a location inside the **United Kingdom** can lock the account until you
> submit age-verification documents — avoid UK coordinates.

Typical flow: `grindr_login` → `grindr_list_conversations` →
`grindr_get_messages`, or `grindr_list_tags` / `grindr_list_endpoints` →
`grindr_describe_endpoint` → `grindr_request` for everything else.

## Install (prebuilt binary — no Rust toolchain needed)

Each tagged release publishes standalone binaries built by GitHub Actions, so
you don't need Rust or CMake to run the server. Grab the archive for your
platform from the [Releases page](https://github.com/aekrylov/grindr-mcp/releases):

| Platform | Asset |
| --- | --- |
| macOS, Apple Silicon | `grindr-mcp-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `grindr-mcp-x86_64-apple-darwin.tar.gz` |
| Linux, x86-64 | `grindr-mcp-x86_64-unknown-linux-gnu.tar.gz` |

```sh
# Example: macOS Apple Silicon
tar -xzf grindr-mcp-aarch64-apple-darwin.tar.gz
sudo mv grindr-mcp-aarch64-apple-darwin/grindr-mcp /usr/local/bin/
xattr -d com.apple.quarantine /usr/local/bin/grindr-mcp 2>/dev/null || true  # macOS Gatekeeper
```

Each archive has a matching `.sha256` you can verify with `shasum -a 256 -c`.

## Build from source

Only needed if you want to build it yourself. Requires Rust and CMake (the
latter for the BoringSSL used by the TLS fingerprint).

```sh
brew install rust cmake     # if you don't have them
cargo build --release       # binary at target/release/grindr-mcp
```

## Configure

The server speaks MCP over **stdio**. Point your client at the built binary.

### Claude Code

```sh
claude mcp add grindr -- /Users/anth/prog/grindr-mcp/target/release/grindr-mcp
```

### Claude Desktop / generic `mcpServers` config

```json
{
  "mcpServers": {
    "grindr": {
      "command": "/Users/anth/prog/grindr-mcp/target/release/grindr-mcp"
    }
  }
}
```

Then ask the client to log in (it will call `grindr_login` with your
credentials), or pre-seed a session in the state file.

## State & secrets

The device identity and session are stored at:

- `$GRINDR_MCP_STATE` if set, otherwise
- `<config dir>/grindr-mcp/state.json` (on macOS:
  `~/Library/Application Support/grindr-mcp/state.json`).

The file is written `0600`. It contains bearer tokens — treat it as a secret.
Keeping the same device across runs is less likely to trip Cloudflare, so the
generated `DeviceInfo` is reused once created.

## How requests work

`grindr_request` maps directly onto the endpoints in the bundled
`openapi.json`. For example:

- `GET /v3/me/profile` — your own profile
- `GET /v4/cascade` — the browse grid
- `GET /v5/rightnow/feed` — Right Now feed

The session token (`Authorization: Grindr3 <jwt>`), device headers and the
okhttp TLS/HTTP2 fingerprint are all applied automatically; the token is
refreshed before it expires and once reactively on a `401`.

## Layout

- `src/main.rs` — server struct, shared request helpers, `ServerHandler` and entrypoint.
- `src/tools/generic.rs` — generic discovery / request tools (`grindr_request`, `grindr_list_endpoints`, `grindr_describe_endpoint`, `grindr_list_tags`).
- `src/tools/endpoints.rs` — endpoint-specific tools (auth, conversations, messages, location, grid, profile).
- `src/openapi.rs` — discovery helpers over the bundled spec.
- `src/geohash.rs` — geohash encoding for location tools.
- `src/state.rs` — on-disk device + session + pinned-location persistence.
- `openapi.json` — Grindr OpenAPI spec (from <https://opengrind.org/openapi.json>).

The two tool groups are separate `#[tool_router]` blocks (`generic_router` and
`endpoint_router`) merged in `GrindrServer::new`. The transport crate
[`grindr.rs`](https://git.opengrind.org/open-grind/grindr.rs) is a pinned git
dependency.

## Credits

- [`grindr.rs`](https://git.opengrind.org/open-grind/grindr.rs) — the transport.
- [Open Grind](https://opengrind.org) — the API reference and OpenAPI spec.
