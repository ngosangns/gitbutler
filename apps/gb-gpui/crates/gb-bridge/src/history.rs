//! Manual snapshots and the file summary of one oplog entry.
//!
//! Listing and restore already live in [`crate::mutate`]. A snapshot records
//! the project before its operation, so the diff is that tree against the
//! next snapshot (or the current workspace commit, when this entry is the
//! newest). Restoring still goes through `restore_snapshot`, which writes a
//! new snapshot and can itself be undone.

use but_core::ui::TreeStatus;

use crate::mutate::{self, WriteGate};
use crate::process::Hub;
use crate::read::VersionSource;
use crate::{FileStatus, Result, SnapshotChange, SnapshotInfo, lossy_bytes};

pub(crate) fn snapshot_diff(
    source: VersionSource,
    id: &str,
    sha: &str,
) -> Result<Vec<SnapshotChange>> {
    let oid = mutate::parse_sha(sha)?;
    let ctx = crate::live::open_context(source, id)?;
    let changes = but_api::legacy::oplog::snapshot_diff(&ctx, oid, None)
        .map_err(|error| mutate::api_error("diff this snapshot", error))?;
    Ok(changes.into_iter().map(change_of).collect())
}

pub(crate) fn create_snapshot(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    message: &str,
) -> Result<SnapshotInfo> {
    let text = message.trim();
    let message = if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    };
    let mut gate = WriteGate::open(source, id, hub)?;
    let oid = but_api::legacy::oplog::create_snapshot(gate.ctx(), message)
        .map_err(|error| mutate::api_error("create a snapshot", error))?;
    drop(gate);
    let sha = oid.to_string();
    mutate::snapshots(source, id, 50)?
        .into_iter()
        .find(|snapshot| snapshot.sha == sha)
        .ok_or_else(|| {
            crate::BridgeError::internal("The snapshot was created but could not be read back.")
        })
}

fn change_of(change: but_core::ui::TreeChange) -> SnapshotChange {
    let path = lossy_bytes(&change.path_bytes);
    let (status, old_path) = match change.status {
        TreeStatus::Addition { .. } => (FileStatus::Added, None),
        TreeStatus::Deletion { .. } => (FileStatus::Deleted, None),
        TreeStatus::Modification { .. } => (FileStatus::Modified, None),
        TreeStatus::Rename {
            previous_path_bytes,
            ..
        } => (FileStatus::Renamed, Some(lossy_bytes(&previous_path_bytes))),
    };
    SnapshotChange {
        path,
        old_path,
        status,
    }
}
