//! Read-only helpers over the bundled Grindr OpenAPI document.
//!
//! The spec (`openapi.json`, fetched from <https://opengrind.org/openapi.json>)
//! is compiled into the binary so the discovery tools work offline. These
//! helpers power `grindr_list_endpoints` and `grindr_describe_endpoint`, which
//! let an agent find the right path/method before calling `grindr_request`.

use std::sync::LazyLock;

use serde_json::{json, Map, Value};

/// The parsed OpenAPI document, compiled in at build time.
pub static SPEC: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../openapi.json")).expect("bundled openapi.json is valid")
});

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

/// A short description of the API the spec covers (title/version/servers).
pub fn api_info() -> Value {
    let info = SPEC.get("info").cloned().unwrap_or(Value::Null);
    let servers = SPEC
        .get("servers")
        .and_then(|s| s.as_array())
        .map(|a| a.iter().filter_map(|s| s.get("url").cloned()).collect::<Vec<_>>())
        .unwrap_or_default();
    json!({ "info": info, "servers": servers })
}

/// List the operations in the spec, optionally filtered by `tag` (exact match)
/// and/or `search` (case-insensitive substring over path, summary,
/// description and operationId).
pub fn list_endpoints(tag: Option<&str>, search: Option<&str>) -> Vec<Value> {
    let needle = search.map(|s| s.to_lowercase());
    let mut out = Vec::new();

    let Some(paths) = SPEC.get("paths").and_then(|p| p.as_object()) else {
        return out;
    };

    for (path, item) in paths {
        let Some(item) = item.as_object() else { continue };
        for &method in HTTP_METHODS {
            let Some(op) = item.get(method) else { continue };

            if let Some(tag) = tag {
                let has_tag = op
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|a| a.iter().any(|t| t.as_str() == Some(tag)))
                    .unwrap_or(false);
                if !has_tag {
                    continue;
                }
            }

            if let Some(needle) = &needle {
                let hay = [
                    path.as_str(),
                    op.get("summary").and_then(|s| s.as_str()).unwrap_or(""),
                    op.get("description").and_then(|s| s.as_str()).unwrap_or(""),
                    op.get("operationId").and_then(|s| s.as_str()).unwrap_or(""),
                ]
                .join(" ")
                .to_lowercase();
                if !hay.contains(needle.as_str()) {
                    continue;
                }
            }

            let mut entry = Map::new();
            entry.insert("method".into(), json!(method.to_uppercase()));
            entry.insert("path".into(), json!(path));
            if let Some(s) = op.get("summary") {
                entry.insert("summary".into(), s.clone());
            }
            if let Some(t) = op.get("tags") {
                entry.insert("tags".into(), t.clone());
            }
            for flag in ["deprecated", "x-wip", "x-paid", "x-legacy"] {
                if op.get(flag).and_then(|v| v.as_bool()) == Some(true) {
                    entry.insert(flag.into(), json!(true));
                }
            }
            let auth_required = op
                .get("security")
                .map(|s| !matches!(s, Value::Array(a) if a.is_empty()))
                .unwrap_or(true);
            entry.insert("auth_required".into(), json!(auth_required));
            out.push(Value::Object(entry));
        }
    }

    out.sort_by(|a, b| {
        let pa = a.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let pb = b.get("path").and_then(|v| v.as_str()).unwrap_or("");
        pa.cmp(pb).then_with(|| {
            let ma = a.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let mb = b.get("method").and_then(|v| v.as_str()).unwrap_or("");
            ma.cmp(mb)
        })
    });
    out
}

/// The list of tag names with their descriptions.
pub fn list_tags() -> Vec<Value> {
    SPEC.get("tags")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Describe a single operation: the operation object with `$ref`s expanded
/// (bounded depth, cycle-guarded) so parameter and schema details are inlined.
///
/// `method` is optional; when omitted, every method defined on the path is
/// returned.
pub fn describe_endpoint(path: &str, method: Option<&str>) -> Option<Value> {
    let item = SPEC.get("paths").and_then(|p| p.get(path))?.as_object()?;

    let mut ops = Map::new();
    for &m in HTTP_METHODS {
        if let Some(want) = method {
            if !want.eq_ignore_ascii_case(m) {
                continue;
            }
        }
        if let Some(op) = item.get(m) {
            let mut seen = Vec::new();
            ops.insert(m.to_uppercase(), resolve_refs(op, 6, &mut seen));
        }
    }

    if ops.is_empty() {
        return None;
    }
    Some(json!({ "path": path, "operations": ops }))
}

/// Resolve a JSON Pointer like `#/components/schemas/Foo` against the spec.
fn resolve_pointer(reference: &str) -> Option<Value> {
    let rest = reference.strip_prefix("#/")?;
    let mut node = &*SPEC;
    for raw in rest.split('/') {
        let key = raw.replace("~1", "/").replace("~0", "~");
        node = node.get(&key)?;
    }
    Some(node.clone())
}

/// Recursively inline `$ref`s up to `depth`, tracking the active ref chain in
/// `seen` to break reference cycles (common in recursive schemas).
fn resolve_refs(node: &Value, depth: usize, seen: &mut Vec<String>) -> Value {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                if depth == 0 || seen.contains(reference) {
                    return node.clone();
                }
                if let Some(target) = resolve_pointer(reference) {
                    seen.push(reference.clone());
                    let resolved = resolve_refs(&target, depth - 1, seen);
                    seen.pop();
                    return resolved;
                }
                return node.clone();
            }
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), resolve_refs(v, depth, seen));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| resolve_refs(v, depth, seen)).collect())
        }
        other => other.clone(),
    }
}
