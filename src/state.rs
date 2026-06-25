//! On-disk persistence for the device identity and the current session.
//!
//! Keeping the same [`DeviceInfo`] across runs is less likely to trip
//! Cloudflare, and persisting the [`Session`] means the user does not have to
//! log in again every time the server restarts. Both are secrets, so the file
//! is written with `0600` permissions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use grindr::{DeviceInfo, Session};
use serde::{Deserialize, Serialize};

/// Everything we keep on disk between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub device: DeviceInfo,
    #[serde(default)]
    pub session: Option<Session>,
}

/// Resolve the state file path: `$GRINDR_MCP_STATE` if set, otherwise
/// `<config dir>/grindr-mcp/state.json`.
pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("GRINDR_MCP_STATE") {
        return PathBuf::from(p);
    }
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("grindr-mcp").join("state.json")
}

/// Load existing state, or create a fresh one with a newly generated device.
///
/// Returns the state and whether it was freshly generated (so the caller can
/// persist it immediately).
pub fn load_or_init(path: &Path) -> Result<(PersistedState, bool)> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let state: PersistedState = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing state file {}", path.display()))?;
            Ok((state, false))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let state = PersistedState {
                device: DeviceInfo::generate(),
                session: None,
            };
            Ok((state, true))
        }
        Err(e) => Err(e).with_context(|| format!("reading state file {}", path.display())),
    }
}

/// Write the device + session to disk atomically-ish (via a temp file rename),
/// restricting permissions to the owner.
pub fn save(path: &Path, device: &DeviceInfo, session: Option<&Session>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }

    let state = PersistedState {
        device: device.clone(),
        session: session.cloned(),
    };
    let json = serde_json::to_vec_pretty(&state)?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    set_owner_only(&tmp)?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}
