//! Reads that go through `but_api` / `but_ctx`.
//!
//! Each one calls [`crate::read::prepared_project`] first, which checks the
//! version pin, the release channel, `projects.json`, `settings.json`, and
//! that no migration is pending. Only then is a `Context` opened.
//! `Context::new_from_legacy_project_id` loads settings, and that load can
//! create `settings.json` or rewrite it for a legacy Tauri-store migration.
//! The preflight refuses those cases before the load runs.

use but_core::UnifiedPatch;
use but_ctx::Context;

use crate::read::{self, VersionSource};
use crate::{
    AssignedHunk, BridgeError, CommitDetails, CommitFile, DiffRequest, FileChange, FileStatus,
    Result, TextDiff, WorktreeView, branch_label, lossy_bytes, short_sha,
};

pub(crate) fn worktree(source: VersionSource, id: &str) -> Result<WorktreeView> {
    let ctx = open_context(source, id)?;
    // `compute_deps_and_assignments: true` persists hunk assignments
    // (`but-api/src/diff.rs`, `immediate_transaction` + `commit`). The watcher
    // emits `Change::WorktreeChanges` only after a filesystem event
    // (`gitbutler-watcher/src/handler.rs`); it does not scan once at startup.
    // Passing `false` returns the file list and leaves `assignments` empty
    // without opening the assignment table. Line counts come from read-only
    // unified patches. Lane chips arrive with the watcher's first
    // `WorktreeChanges` payload, which is applied directly.
    let mut changes =
        but_api::diff::changes_in_worktree(&ctx, but_api::commit::json::ChangesSource::Head, false)
            .map_err(|error| {
                BridgeError::internal(format!("Could not read uncommitted changes: {error}"))
            })?;
    // `false` leaves `assignments` empty and does not write. Saved rows come
    // from a read-only connection, not `ctx.db` (that opens a writer).
    if changes.assignments.is_empty() {
        changes.assignments = persisted_assignments(&ctx)?;
    }
    let mut view = view_from_changes(&changes, Vec::new());
    fill_line_counts(&ctx, &changes, &mut view)?;
    Ok(view)
}

pub(crate) fn commit_details(source: VersionSource, id: &str, sha: &str) -> Result<CommitDetails> {
    let ctx = open_context(source, id)?;
    let oid = parse_sha(sha)?;
    let details = but_api::diff::commit_details(&ctx, oid, but_api::diff::ComputeLineStats::Yes)
        .map_err(|error| BridgeError::not_found(format!("Could not read commit {sha}: {error}")))?;
    let repo = ctx.repo.get().map_err(|error| {
        BridgeError::internal(format!("Could not open the repository: {error}"))
    })?;
    let lines = ctx.settings.context_lines;
    let files = details
        .diff_with_first_parent
        .iter()
        .map(|change| commit_file(&repo, lines, change))
        .collect::<Result<Vec<_>>>()?;
    let commit = &details.commit;
    let message = lossy_bytes(&commit.inner.message);
    Ok(CommitDetails {
        sha: commit.id.to_hex().to_string(),
        short_sha: short_sha(commit.id),
        author: signature_label(&commit.inner.author),
        committer: signature_label(&commit.inner.committer),
        authored_at: commit.inner.author.time.seconds,
        committed_at: commit.inner.committer.time.seconds,
        parents: commit
            .inner
            .parents
            .iter()
            .map(|parent| parent.to_hex().to_string())
            .collect(),
        message,
        files,
    })
}

pub(crate) fn file_diff(source: VersionSource, id: &str, request: DiffRequest) -> Result<TextDiff> {
    let ctx = open_context(source, id)?;
    match request {
        DiffRequest::Worktree { path } => {
            let changes = but_api::diff::changes_in_worktree(
                &ctx,
                but_api::commit::json::ChangesSource::Head,
                false,
            )
            .map_err(|error| {
                BridgeError::internal(format!("Could not read uncommitted changes: {error}"))
            })?;
            let change = changes
                .worktree_changes
                .changes
                .into_iter()
                .find(|change| lossy_bytes(AsRef::<[u8]>::as_ref(&change.path)) == path)
                .ok_or_else(|| {
                    BridgeError::not_found(format!("No uncommitted change for {path}"))
                })?;
            let patch = but_api::diff::tree_change_diffs(&ctx, change).map_err(|error| {
                BridgeError::internal(format!("Could not diff {path}: {error}"))
            })?;
            Ok(text_diff(path, patch))
        }
        DiffRequest::Commit { sha, path } => {
            let oid = parse_sha(&sha)?;
            let details =
                but_api::diff::commit_details(&ctx, oid, but_api::diff::ComputeLineStats::No)
                    .map_err(|error| {
                        BridgeError::not_found(format!("Could not read commit {sha}: {error}"))
                    })?;
            let change = details
                .diff_with_first_parent
                .into_iter()
                .find(|change| lossy_bytes(&change.path) == path)
                .ok_or_else(|| {
                    BridgeError::not_found(format!("{path} is not part of commit {sha}"))
                })?;
            let repo = ctx.repo.get().map_err(|error| {
                BridgeError::internal(format!("Could not open the repository: {error}"))
            })?;
            let patch = change
                .unified_patch(&repo, ctx.settings.context_lines)
                .map_err(|error| {
                    BridgeError::internal(format!("Could not diff {path}: {error}"))
                })?;
            Ok(text_diff(path, patch))
        }
    }
}

pub(crate) fn open_context(source: VersionSource, id: &str) -> Result<Context> {
    read::prepared_project(source, id)?;
    let project_id: but_ctx::ProjectHandleOrLegacyProjectId = id.parse().map_err(|error| {
        BridgeError::not_found(format!("Project id '{id}' is not valid: {error}"))
    })?;
    Context::try_from(project_id)
        .map_err(|error| BridgeError::internal(format!("Could not open the project: {error}")))
}

pub(crate) fn view_from_changes(
    changes: &but_hunk_assignment::WorktreeChanges,
    changed_paths: Vec<String>,
) -> WorktreeView {
    let files = changes
        .worktree_changes
        .changes
        .iter()
        .map(|change| file_from_change(change, &changes.assignments))
        .collect();
    WorktreeView {
        files,
        changed_paths,
    }
}

fn file_from_change(
    change: &but_core::ui::TreeChange,
    assignments: &[but_hunk_assignment::HunkAssignment],
) -> FileChange {
    let path = lossy_bytes(AsRef::<[u8]>::as_ref(&change.path));
    let (status, old_path) = ui_status(&change.status);
    let related: Vec<_> = assignments
        .iter()
        .filter(|assignment| assignment.path == path)
        .collect();
    let additions = related
        .iter()
        .map(|assignment| {
            assignment
                .line_nums_added
                .as_ref()
                .map(|lines| lines.len() as u32)
                .unwrap_or(0)
        })
        .sum();
    let deletions = related
        .iter()
        .map(|assignment| {
            assignment
                .line_nums_removed
                .as_ref()
                .map(|lines| lines.len() as u32)
                .unwrap_or(0)
        })
        .sum();
    let hunks = related
        .iter()
        .map(|assignment| AssignedHunk {
            header: assignment.hunk_header.map(header_text).unwrap_or_default(),
            lane: assignment.branch_ref_bytes.as_ref().and_then(lane_name),
        })
        .collect();
    FileChange {
        path,
        old_path,
        status,
        additions,
        deletions,
        hunks,
    }
}

fn persisted_assignments(ctx: &Context) -> Result<Vec<but_hunk_assignment::HunkAssignment>> {
    let opened = but_db::DbHandle::open_existing_read_only_in_directory(ctx.project_data_dir())
        .map_err(|error| {
            BridgeError::internal(format!("Could not read hunk assignments: {error}"))
        })?;
    let Some(db) = opened else {
        return Ok(Vec::new());
    };
    let rows = db.hunk_assignments().list_all().map_err(|error| {
        BridgeError::internal(format!("Could not read hunk assignments: {error}"))
    })?;
    rows.into_iter()
        .map(|row| {
            but_hunk_assignment::HunkAssignment::try_from(row).map_err(|error| {
                BridgeError::internal(format!("Could not read a hunk assignment: {error}"))
            })
        })
        .collect()
}

fn fill_line_counts(
    ctx: &Context,
    changes: &but_hunk_assignment::WorktreeChanges,
    view: &mut WorktreeView,
) -> Result<()> {
    let repo = ctx.repo.get().map_err(|error| {
        BridgeError::internal(format!("Could not open the repository: {error}"))
    })?;
    let lines = ctx.settings.context_lines;
    for file in &mut view.files {
        if file.additions > 0 || file.deletions > 0 {
            continue;
        }
        let Some(change) = changes
            .worktree_changes
            .changes
            .iter()
            .find(|change| lossy_bytes(AsRef::<[u8]>::as_ref(&change.path)) == file.path)
        else {
            continue;
        };
        let core: but_core::TreeChange = change.clone().into();
        let patch = core.unified_patch(&repo, lines).map_err(|error| {
            BridgeError::internal(format!("Could not count lines in {}: {error}", file.path))
        })?;
        if let Some(UnifiedPatch::Patch {
            lines_added,
            lines_removed,
            ..
        }) = patch
        {
            file.additions = lines_added;
            file.deletions = lines_removed;
        }
    }
    Ok(())
}

fn commit_file(
    repo: &gix::Repository,
    lines: u32,
    change: &but_core::TreeChange,
) -> Result<CommitFile> {
    let path = lossy_bytes(&change.path);
    let status = core_status(&change.status);
    let patch = change.unified_patch(repo, lines).map_err(|error| {
        BridgeError::internal(format!("Could not count lines in {path}: {error}"))
    })?;
    let (additions, deletions) = match patch {
        Some(UnifiedPatch::Patch {
            lines_added,
            lines_removed,
            ..
        }) => (lines_added, lines_removed),
        _ => (0, 0),
    };
    Ok(CommitFile {
        path,
        status,
        additions,
        deletions,
    })
}

fn text_diff(path: String, patch: Option<UnifiedPatch>) -> TextDiff {
    match patch {
        Some(UnifiedPatch::Binary) => TextDiff {
            path,
            patch: String::new(),
            binary: true,
            too_large: false,
        },
        None => TextDiff {
            path,
            patch: String::new(),
            binary: false,
            too_large: false,
        },
        Some(UnifiedPatch::TooLarge { .. }) => TextDiff {
            path,
            patch: String::new(),
            binary: false,
            too_large: true,
        },
        Some(UnifiedPatch::Patch { hunks, .. }) => {
            let mut text = String::new();
            for hunk in hunks {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&lossy_bytes(&hunk.diff));
            }
            TextDiff {
                path,
                patch: text,
                binary: false,
                too_large: false,
            }
        }
    }
}

fn header_text(header: but_core::HunkHeader) -> String {
    format!(
        "@@ -{},{} +{},{} @@",
        header.old_start, header.old_lines, header.new_start, header.new_lines
    )
}

fn ui_status(status: &but_core::ui::TreeStatus) -> (FileStatus, Option<String>) {
    match status {
        but_core::ui::TreeStatus::Addition { .. } => (FileStatus::Added, None),
        but_core::ui::TreeStatus::Deletion { .. } => (FileStatus::Deleted, None),
        but_core::ui::TreeStatus::Modification { .. } => (FileStatus::Modified, None),
        but_core::ui::TreeStatus::Rename { previous_path, .. } => (
            FileStatus::Renamed,
            Some(lossy_bytes(AsRef::<[u8]>::as_ref(previous_path))),
        ),
    }
}

fn core_status(status: &but_core::TreeStatus) -> FileStatus {
    match status {
        but_core::TreeStatus::Addition { .. } => FileStatus::Added,
        but_core::TreeStatus::Deletion { .. } => FileStatus::Deleted,
        but_core::TreeStatus::Modification { .. } => FileStatus::Modified,
        but_core::TreeStatus::Rename { .. } => FileStatus::Renamed,
    }
}

fn lane_name(name: &gix::refs::FullName) -> Option<String> {
    let label = branch_label(name.as_ref());
    if label.is_empty() { None } else { Some(label) }
}

fn signature_label(signature: &gix::actor::Signature) -> String {
    let name = lossy_bytes(&signature.name);
    let email = lossy_bytes(&signature.email);
    if email.is_empty() {
        name
    } else {
        format!("{name} <{email}>")
    }
}

fn parse_sha(sha: &str) -> Result<gix::ObjectId> {
    gix::ObjectId::from_hex(sha.as_bytes())
        .map_err(|_| BridgeError::not_found(format!("'{sha}' is not a commit id")))
}
