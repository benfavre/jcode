use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use jcode_operator_backend::OperatorBackend;
use jcode_operator_backend::platform_contract::ClientId;
use serde::{Deserialize, Serialize};

use super::state::{CockpitState, MAX_PANES, PaneLayout, View};

const WORKSPACE_VERSION: u32 = 1;
const MAX_WORKSPACE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedWorkspace {
    version: u32,
    layout: String,
    panes: Vec<SavedPane>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedPane {
    session_id: String,
    pinned: bool,
}

pub(super) async fn restore_workspace(
    backend: &impl OperatorBackend,
    client: &ClientId,
    state: &mut CockpitState,
) -> Result<(), &'static str> {
    let path = workspace_path().ok_or("config_directory_unavailable")?;
    let Some(saved) = read_workspace(&path)? else {
        return Ok(());
    };
    if saved.version != WORKSPACE_VERSION || saved.panes.len() > MAX_PANES {
        return Err("workspace_version_or_size_invalid");
    }
    state.pane_layout = PaneLayout::parse(&saved.layout).ok_or("workspace_layout_invalid")?;
    let mut restored = BTreeSet::new();
    for wanted in saved.panes {
        if !restored.insert(wanted.session_id.clone()) {
            return Err("workspace_duplicate_session");
        }
        let Some(record) = state
            .overview
            .sessions
            .iter()
            .find(|record| record.session.resource.id.as_str() == wanted.session_id)
            .cloned()
        else {
            continue;
        };
        if !record.attachable {
            continue;
        }
        let Ok(attachment) = backend
            .attach(record.session.resource.clone(), client.clone())
            .await
        else {
            continue;
        };
        if state.attach(record, attachment)
            && let Some(pane) = state.panes.last_mut()
        {
            pane.pinned = wanted.pinned;
        }
    }
    if !state.panes.is_empty() {
        state.set_view(View::Sessions);
        state.status = String::from("saved observer panes restored without control authority");
    }
    Ok(())
}

fn read_workspace(path: &Path) -> Result<Option<SavedWorkspace>, &'static str> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("workspace_unreadable"),
    };
    if !metadata.is_file() || metadata.len() > MAX_WORKSPACE_BYTES {
        return Err("workspace_file_invalid");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o600 {
            return Err("workspace_permissions_invalid");
        }
    }
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| "workspace_unreadable")?
        .take(MAX_WORKSPACE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "workspace_unreadable")?;
    if bytes.len() as u64 > MAX_WORKSPACE_BYTES {
        return Err("workspace_too_large");
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "workspace_json_invalid")
}

pub(super) fn save_workspace(state: &CockpitState) -> Result<(), &'static str> {
    let path = workspace_path().ok_or("config_directory_unavailable")?;
    let parent = path.parent().ok_or("config_directory_unavailable")?;
    std::fs::create_dir_all(parent).map_err(|_| "workspace_directory_failed")?;
    crate::platform::set_directory_permissions_owner_only(parent)
        .map_err(|_| "workspace_permissions_failed")?;
    let saved = SavedWorkspace {
        version: WORKSPACE_VERSION,
        layout: state.pane_layout.label().to_owned(),
        panes: state
            .panes
            .iter()
            .map(|pane| SavedPane {
                session_id: pane.record.session.resource.id.as_str().to_owned(),
                pinned: pane.pinned,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(|_| "workspace_encode_failed")?;
    if bytes.len() as u64 > MAX_WORKSPACE_BYTES {
        return Err("workspace_too_large");
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "workspace_clock_invalid")?
        .as_nanos();
    let temporary = parent.join(format!(
        ".platform-workspace.{}.{nonce}.tmp",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| "workspace_create_failed")?;
    let result = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| crate::platform::set_permissions_owner_only(&temporary))
        .and_then(|()| std::fs::rename(&temporary, &path));
    if result.is_err() {
        if std::fs::remove_file(&temporary).is_err() && temporary.exists() {
            return Err("workspace_cleanup_failed");
        }
        return Err("workspace_save_failed");
    }
    Ok(())
}

fn workspace_path() -> Option<PathBuf> {
    dirs::config_dir().map(|root| root.join("jcode/platform-workspace.json"))
}
