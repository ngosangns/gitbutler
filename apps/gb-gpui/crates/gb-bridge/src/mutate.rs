//! Mutations through `but_api`. Each one opens a `Context` only after preflight.
//!
//! `commit_create` is the one call that does not take the worktree write lock
//! itself, so this module holds `exclusive_worktree_access` across that call.
//! Every other mutation takes that lock internally. Holding it across those
//! calls deadlocks, so stack ids and hunk headers are resolved first.

use but_core::UnifiedPatch;
use but_core::ref_metadata::StackId;
use but_core::tree::create_tree::RejectionReason;
use but_ctx::Context;
use but_hunk_assignment::{HunkAssignmentRequest, HunkAssignmentTarget};
use gix::bstr::ByteSlice as _;

use crate::process::Hub;
use crate::read::{self, VersionSource};
use crate::{
    Anchor, BridgeError, CommitResult, DiscardResult, HookReport, HookStatus, LaneCreated, Result,
    SelectionSpec, SnapshotInfo, UndoResult, WorkspaceSummary, branch_label, lossy_bytes,
};

const NOT_EXCLUSIVE_MESSAGE: &str = "Another GitButler window has this project open.";
const NOT_EXCLUSIVE_HINT: &str = "Close the other GitButler window, then try again.";

pub(crate) struct WriteGate {
    ctx: Context,
    /// `project.lock` for this mutation. `None` when a watch in this process
    /// already holds it. A second flock in the same process is not reliable:
    /// closing either descriptor can drop the lock for the whole process.
    _lock: Option<but_core::sync::LockFile>,
}

impl WriteGate {
    pub(crate) fn open(source: VersionSource, id: &str, hub: &Hub) -> Result<Self> {
        let gate = Self::open_unlocked(source, id, hub)?;
        crate::edit::refuse_if_editing(&gate.ctx)?;
        Ok(gate)
    }

    /// Save and abort are the writes that must run while edit mode is on.
    pub(crate) fn open_during_edit(source: VersionSource, id: &str, hub: &Hub) -> Result<Self> {
        Self::open_unlocked(source, id, hub)
    }

    fn open_unlocked(source: VersionSource, id: &str, hub: &Hub) -> Result<Self> {
        let mut ctx = crate::live::open_context(source, id)?;
        if hub.owns(id) {
            return Ok(Self { ctx, _lock: None });
        }
        match ctx.try_exclusive_access() {
            Ok(lock) => Ok(Self {
                ctx,
                _lock: Some(lock),
            }),
            Err(error) => {
                if hub.owns(id) {
                    Ok(Self { ctx, _lock: None })
                } else if is_not_exclusive(&error) {
                    Err(not_exclusive())
                } else {
                    Err(api_error("lock this project", error))
                }
            }
        }
    }

    pub(crate) fn ctx(&mut self) -> &mut Context {
        &mut self.ctx
    }
}

pub(crate) fn assign(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    selections: &[SelectionSpec],
    lane: Option<&str>,
) -> Result<WorkspaceSummary> {
    require_selection(selections)?;
    let workspace = {
        let mut gate = WriteGate::open(source, id, hub)?;
        let stack_id = match lane {
            Some(name) => Some(stack_for_lane(&gate.ctx, name)?),
            None => None,
        };
        let requests = assignment_requests(&gate.ctx, selections, stack_id)?;
        but_api::diff::assign_hunk(&mut gate.ctx, requests)
            .map_err(|error| api_error("assign these changes", error))?;
        drop(gate);
        read::open_project(source, id)?
    };
    Ok(workspace)
}

pub(crate) fn discard(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    selections: &[SelectionSpec],
) -> Result<DiscardResult> {
    require_selection(selections)?;
    let specs = diff_specs(selections);
    let mut gate = WriteGate::open(source, id, hub)?;
    let refused = but_api::legacy::workspace::discard_worktree_changes(&mut gate.ctx, specs)
        .map_err(|error| api_error("discard these changes", error))?;
    drop(gate);
    let workspace = read::open_project(source, id)?;
    Ok(DiscardResult {
        workspace,
        refused: refused
            .into_iter()
            .map(|spec| lossy_bytes(&spec.path))
            .collect(),
    })
}

pub(crate) fn create_lane(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    name: &str,
) -> Result<LaneCreated> {
    let full = local_ref(name)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let created = but_api::branch::branch_create(
        &mut gate.ctx,
        Some(full),
        but_api::branch::json::BranchCreatePlacement::Independent,
    )
    .map_err(|error| api_error("create a lane", error))?;
    let workspace = read::summarize(id, &created.workspace.head_info);
    let name = branch_label(created.new_ref.as_ref());
    Ok(LaneCreated { workspace, name })
}

pub(crate) fn commit_create(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    anchor: &Anchor,
    selections: &[SelectionSpec],
    message: &str,
) -> Result<CommitResult> {
    require_selection(selections)?;
    require_message(message)?;
    let (relative, side) = placement(anchor)?;
    let specs = diff_specs(selections);
    let mut gate = WriteGate::open(source, id, hub)?;
    let mut guard = gate.ctx.exclusive_worktree_access();
    let created = but_api::commit::create::commit_create(
        &mut gate.ctx,
        relative,
        side,
        specs,
        but_api::commit::json::ChangesSource::Head,
        message.to_string(),
        but_core::DryRun::No,
        guard.write_permission(),
    )
    .map_err(|error| api_error("create a commit", error))?;
    Ok(commit_result(
        id,
        created.workspace,
        created.new_commit,
        created.rejected_specs,
    ))
}

pub(crate) fn amend(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    sha: &str,
    selections: &[SelectionSpec],
) -> Result<CommitResult> {
    require_selection(selections)?;
    let oid = parse_sha(sha)?;
    let specs = diff_specs(selections);
    let mut gate = WriteGate::open(source, id, hub)?;
    let amended = but_api::commit::amend::commit_amend(
        &mut gate.ctx,
        oid,
        specs,
        but_api::commit::json::ChangesSource::Head,
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("amend this commit", error))?;
    Ok(commit_result(
        id,
        amended.workspace,
        amended.new_commit,
        amended.rejected_specs,
    ))
}

pub(crate) fn reword(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    sha: &str,
    message: &str,
) -> Result<CommitResult> {
    require_message(message)?;
    let oid = parse_sha(sha)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let reworded = but_api::commit::reword::commit_reword(
        &mut gate.ctx,
        oid,
        gix::bstr::BString::from(message.to_string()),
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("reword this commit", error))?;
    Ok(commit_result(
        id,
        reworded.workspace,
        Some(reworded.new_commit),
        Vec::new(),
    ))
}

pub(crate) fn squash(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    subject_shas: &[String],
    target_sha: &str,
) -> Result<CommitResult> {
    if subject_shas.is_empty() {
        return Err(BridgeError::not_ready("Choose a commit to squash."));
    }
    let subjects = subject_shas
        .iter()
        .map(|sha| parse_sha(sha))
        .collect::<Result<Vec<_>>>()?;
    let target = parse_sha(target_sha)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let squashed = but_api::commit::squash::commit_squash(
        &mut gate.ctx,
        subjects,
        target,
        but_workspace::commit::squash_commits::MessageCombinationStrategy::KeepBoth,
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("squash these commits", error))?;
    Ok(commit_result(
        id,
        squashed.workspace,
        Some(squashed.new_commit),
        Vec::new(),
    ))
}

pub(crate) fn uncommit(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    shas: &[String],
    lane: Option<&str>,
) -> Result<CommitResult> {
    if shas.is_empty() {
        return Err(BridgeError::not_ready("Choose a commit to uncommit."));
    }
    let subjects = shas
        .iter()
        .map(|sha| parse_sha(sha))
        .collect::<Result<Vec<_>>>()?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let assign_to = match lane {
        Some(name) => Some(stack_for_lane(&gate.ctx, name)?),
        None => None,
    };
    let outcome = but_api::commit::uncommit::commit_uncommit(
        &mut gate.ctx,
        subjects,
        assign_to,
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("uncommit", error))?;
    Ok(commit_result(id, outcome.workspace, None, Vec::new()))
}

pub(crate) fn uncommit_changes(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    sha: &str,
    selections: &[SelectionSpec],
    lane: Option<&str>,
) -> Result<CommitResult> {
    require_selection(selections)?;
    let oid = parse_sha(sha)?;
    let specs = diff_specs(selections);
    let mut gate = WriteGate::open(source, id, hub)?;
    let assign_to = match lane {
        Some(name) => Some(stack_for_lane(&gate.ctx, name)?),
        None => None,
    };
    let outcome = but_api::commit::uncommit::commit_uncommit_changes(
        &mut gate.ctx,
        oid,
        specs,
        assign_to,
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("uncommit these changes", error))?;
    Ok(commit_result(id, outcome.workspace, None, Vec::new()))
}

pub(crate) fn move_branch(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    name: &str,
    anchor: &crate::BranchAnchor,
) -> Result<CommitResult> {
    // This GitButler build stacks the subject on top of a branch. A drop below
    // another branch is planned as a stack onto the branch under it.
    let crate::BranchAnchor::Reference {
        name: onto,
        below: false,
    } = anchor
    else {
        return Err(BridgeError::not_ready(
            "Stack this branch on top of another branch.",
        ));
    };
    let subject = local_ref(name)?;
    let onto = local_ref(onto)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let moved = but_api::branch::move_branch(
        &mut gate.ctx,
        subject.as_ref(),
        onto.as_ref(),
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("move this branch", error))?;
    Ok(commit_result(id, moved.workspace, None, Vec::new()))
}

pub(crate) fn move_commit(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    shas: &[String],
    anchor: &Anchor,
) -> Result<CommitResult> {
    if shas.is_empty() {
        return Err(BridgeError::not_ready("Choose a commit to move."));
    }
    let subjects = shas
        .iter()
        .map(|sha| parse_sha(sha))
        .collect::<Result<Vec<_>>>()?;
    let (relative, side) = placement(anchor)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let moved = but_api::commit::move_commit::commit_move(
        &mut gate.ctx,
        subjects,
        relative,
        side,
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("move this commit", error))?;
    Ok(commit_result(id, moved.workspace, None, Vec::new()))
}

pub(crate) fn insert_blank(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    anchor: &Anchor,
) -> Result<CommitResult> {
    let (relative, side) = placement(anchor)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let inserted = but_api::commit::insert_blank::commit_insert_blank(
        &mut gate.ctx,
        relative,
        side,
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("insert a blank commit", error))?;
    Ok(commit_result(
        id,
        inserted.workspace,
        Some(inserted.new_commit),
        Vec::new(),
    ))
}

pub(crate) fn move_changes(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    source_sha: &str,
    destination_sha: &str,
    selections: &[SelectionSpec],
) -> Result<CommitResult> {
    require_selection(selections)?;
    let source_id = parse_sha(source_sha)?;
    let destination_id = parse_sha(destination_sha)?;
    let specs = diff_specs(selections);
    let mut gate = WriteGate::open(source, id, hub)?;
    let moved = but_api::commit::move_changes::commit_move_changes_between(
        &mut gate.ctx,
        source_id,
        destination_id,
        specs,
        but_core::DryRun::No,
    )
    .map_err(|error| api_error("move these changes", error))?;
    Ok(commit_result(id, moved.workspace, None, Vec::new()))
}

pub(crate) fn snapshots(
    source: VersionSource,
    id: &str,
    limit: usize,
) -> Result<Vec<SnapshotInfo>> {
    let ctx = crate::live::open_context(source, id)?;
    let listed = but_api::legacy::oplog::list_snapshots(&ctx, limit, None, None, None)
        .map_err(|error| api_error("list snapshots", error))?;
    listed.iter().map(snapshot_info).collect()
}

pub(crate) fn restore_snapshot(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    sha: &str,
) -> Result<UndoResult> {
    let oid = parse_sha(sha)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    but_api::legacy::oplog::restore_snapshot(&mut gate.ctx, oid)
        .map_err(|error| api_error("restore this snapshot", error))?;
    drop(gate);
    Ok(UndoResult {
        workspace: read::open_project(source, id)?,
        restored_sha: oid.to_string(),
    })
}

pub(crate) fn undo_last(source: VersionSource, id: &str, hub: &Hub) -> Result<UndoResult> {
    let mut gate = WriteGate::open(source, id, hub)?;
    let target = but_api::legacy::oplog::get_undo_target_snapshot(&gate.ctx)
        .map_err(|error| api_error("find the undo snapshot", error))?;
    let Some(snapshot) = target else {
        return Err(
            BridgeError::not_found("There is nothing to undo.").with_hint(
                "Undo restores the previous GitButler snapshot. Nothing has been recorded yet.",
            ),
        );
    };
    let sha = snapshot.commit_id;
    but_api::legacy::oplog::restore_snapshot_with_kind(
        &mut gate.ctx,
        but_api::legacy::oplog::RestoreKind::RestoreFromSnapshotViaUndo,
        sha,
    )
    .map_err(|error| api_error("undo", error))?;
    drop(gate);
    Ok(UndoResult {
        workspace: read::open_project(source, id)?,
        restored_sha: sha.to_string(),
    })
}

pub(crate) fn pre_commit_hook(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    selections: &[SelectionSpec],
) -> Result<HookReport> {
    require_selection(selections)?;
    let specs = diff_specs(selections);
    let mut gate = WriteGate::open(source, id, hub)?;
    // Holds the in-process write lock. The hook helper rewrites the index and
    // restores it, including when no hook is configured.
    let _guard = gate.ctx.exclusive_worktree_access();
    let outcome = but_api::legacy::repo::pre_commit_hook_diffspecs(&gate.ctx, specs)
        .map_err(|error| api_error("run the pre-commit hook", error))?;
    hook_report(&outcome)
}

pub(crate) fn post_commit_hook(source: VersionSource, id: &str, hub: &Hub) -> Result<HookReport> {
    let mut gate = WriteGate::open(source, id, hub)?;
    let _guard = gate.ctx.exclusive_worktree_access();
    let outcome = but_api::legacy::repo::post_commit_hook(&gate.ctx)
        .map_err(|error| api_error("run the post-commit hook", error))?;
    hook_report(&outcome)
}

pub(crate) fn message_hook(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    message: &str,
) -> Result<HookReport> {
    require_message(message)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let _guard = gate.ctx.exclusive_worktree_access();
    let outcome = but_api::legacy::repo::message_hook(&gate.ctx, message.to_string())
        .map_err(|error| api_error("run the message hook", error))?;
    hook_report(&outcome)
}

fn commit_result(
    id: &str,
    workspace: but_api::WorkspaceState,
    new_sha: Option<gix::ObjectId>,
    rejected: Vec<(RejectionReason, but_core::DiffSpec)>,
) -> CommitResult {
    CommitResult {
        workspace: read::summarize(id, &workspace.head_info),
        new_sha: new_sha.map(|sha| sha.to_string()),
        rejected: rejected_of(rejected),
    }
}

fn rejected_of(rejected: Vec<(RejectionReason, but_core::DiffSpec)>) -> Vec<crate::RejectedChange> {
    rejected
        .into_iter()
        .map(|(reason, spec)| crate::RejectedChange {
            path: lossy_bytes(&spec.path),
            reason: reason_text(reason),
        })
        .collect()
}

fn reason_text(reason: RejectionReason) -> String {
    match reason {
        RejectionReason::NoEffectiveChanges => "Those changes are already in the commit.".into(),
        RejectionReason::CherryPickMergeConflict => {
            "The change conflicts with the commit it would land on.".into()
        }
        RejectionReason::WorkspaceMergeConflict => {
            "Applying this change conflicts with the workspace.".into()
        }
        RejectionReason::WorkspaceMergeConflictOfUnrelatedFile => {
            "The workspace merge conflicts in a file that was not selected.".into()
        }
        RejectionReason::WorktreeFileMissingForObjectConversion => {
            "The file disappeared before it could be committed.".into()
        }
        RejectionReason::FileToLargeOrBinary => {
            "This file is binary or too large to commit as a hunk.".into()
        }
        RejectionReason::PathNotFoundInBaseTree => {
            "The file is not in the commit this change is based on.".into()
        }
        RejectionReason::UnsupportedDirectoryEntry => {
            "This path is not a file GitButler can commit.".into()
        }
        RejectionReason::UnsupportedTreeEntry => {
            "The existing tree entry is not a file GitButler can update.".into()
        }
        RejectionReason::MissingDiffSpecAssociation => {
            "The selected hunk no longer matches the file.".into()
        }
    }
}

fn assignment_requests(
    ctx: &Context,
    selections: &[SelectionSpec],
    stack_id: Option<StackId>,
) -> Result<Vec<HunkAssignmentRequest>> {
    let target = stack_id.map(|stack_id| HunkAssignmentTarget::Stack { stack_id });
    let mut requests = Vec::new();
    for spec in selections {
        let headers = match &spec.hunks {
            Some(hunks) if !hunks.is_empty() => hunks
                .iter()
                .map(|hunk| Some(hunk.header_value()))
                .collect::<Vec<_>>(),
            _ => headers_for_path(ctx, &spec.path)?,
        };
        for header in headers {
            requests.push(HunkAssignmentRequest {
                hunk_header: header,
                path_bytes: spec.path.clone().into(),
                target: target.clone(),
            });
        }
    }
    Ok(requests)
}

/// Every unified-diff header, or one `None` header for a binary or empty file.
/// `None` is only valid when the file has no hunk headers.
fn headers_for_path(ctx: &Context, path: &str) -> Result<Vec<Option<but_core::HunkHeader>>> {
    let changes =
        but_api::diff::changes_in_worktree(ctx, but_api::commit::json::ChangesSource::Head, false)
            .map_err(|error| api_error("read uncommitted changes", error))?;
    let change = changes
        .worktree_changes
        .changes
        .into_iter()
        .find(|change| lossy_bytes(AsRef::<[u8]>::as_ref(&change.path)) == path)
        .ok_or_else(|| BridgeError::not_found(format!("No uncommitted change for {path}")))?;
    let patch = but_api::diff::tree_change_diffs(ctx, change)
        .map_err(|error| api_error(&format!("diff {path}"), error))?;
    match patch {
        Some(UnifiedPatch::Patch { hunks, .. }) if !hunks.is_empty() => Ok(hunks
            .into_iter()
            .map(|hunk| Some(but_core::HunkHeader::from(hunk)))
            .collect()),
        _ => Ok(vec![None]),
    }
}

pub(crate) fn stack_for_lane(ctx: &Context, lane: &str) -> Result<StackId> {
    let full = local_ref(lane)?;
    let (_guard, _repo, workspace, _db) = ctx
        .workspace_and_db()
        .map_err(|error| api_error("read the workspace", error))?;
    let (stack, _) = workspace
        .find_segment_and_stack_by_refname(full.as_ref())
        .ok_or_else(|| BridgeError::not_found(format!("No lane named {lane}")))?;
    stack
        .id
        .ok_or_else(|| BridgeError::not_found(format!("Lane {lane} has no stack id")))
}

pub(crate) fn local_ref(name: &str) -> Result<gix::refs::FullName> {
    let short =
        but_core::branch::normalize_short_name(gix::bstr::BStr::new(name)).map_err(|error| {
            BridgeError::not_ready(format!("'{name}' is not a branch name: {error}"))
        })?;
    gix::refs::Category::LocalBranch
        .to_full_name(short.as_bstr())
        .map_err(|error| BridgeError::not_ready(format!("'{name}' is not a branch name: {error}")))
}

fn placement(
    anchor: &Anchor,
) -> Result<(
    but_rebase::graph_rebase::mutate::RelativeTo,
    but_rebase::graph_rebase::mutate::InsertSide,
)> {
    use but_rebase::graph_rebase::mutate::{InsertSide, RelativeTo};
    match anchor {
        Anchor::Lane(name) => Ok((RelativeTo::Reference(local_ref(name)?), InsertSide::Below)),
        Anchor::Commit { sha, below } => Ok((
            RelativeTo::Commit(parse_sha(sha)?),
            if *below {
                InsertSide::Below
            } else {
                InsertSide::Above
            },
        )),
    }
}

fn diff_specs(selections: &[SelectionSpec]) -> Vec<but_core::DiffSpec> {
    selections
        .iter()
        .map(|spec| but_core::DiffSpec {
            previous_path: spec.previous_path.clone().map(Into::into),
            path: spec.path.clone().into(),
            hunk_headers: spec
                .hunks
                .as_ref()
                .map(|hunks| {
                    hunks
                        .iter()
                        .copied()
                        .map(crate::HunkRef::header_value)
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

fn snapshot_info(snapshot: &impl serde::Serialize) -> Result<SnapshotInfo> {
    let json = serde_json::to_value(snapshot)
        .map_err(|error| BridgeError::internal(format!("Could not read a snapshot: {error}")))?;
    let sha = json
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let created_at_ms = json
        .get("createdAt")
        .and_then(|value| value.as_i64())
        .map(i128::from)
        .unwrap_or(0);
    let details = json.get("details");
    let operation = details
        .and_then(|details| details.get("operation"))
        .and_then(|value| value.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let stored_title = details
        .and_then(|details| details.get("title"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let body = details
        .and_then(|details| details.get("body"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let title = if operation == "OnDemandSnapshot" && !body.is_empty() {
        body.to_string()
    } else if stored_title.is_empty() || stored_title == operation {
        human_operation(&operation).to_string()
    } else {
        stored_title.to_string()
    };
    let restorable = !matches!(
        operation.as_str(),
        "RestoreFromSnapshotViaUndo" | "RestoreFromSnapshotViaRedo"
    );
    Ok(SnapshotInfo {
        sha,
        created_at_ms,
        operation,
        title,
        restorable,
    })
}

fn human_operation(operation: &str) -> &'static str {
    match operation {
        "CreateCommit" => "Created commit",
        "CreateBranch" => "Created branch",
        "DiscardChanges" | "Discard" => "Discarded changes",
        "AmendCommit" => "Amended commit",
        "UndoCommit" => "Undid commit",
        "SquashCommit" => "Squashed commit",
        "UpdateCommitMessage" => "Updated commit message",
        "MoveCommit" => "Moved commit",
        "InsertBlankCommit" => "Inserted blank commit",
        "MoveCommitFile" => "Moved changes",
        "MoveHunk" => "Moved hunk",
        "RestoreFromSnapshotViaUndo" => "Undid",
        "RestoreFromSnapshotViaRedo" => "Redid",
        "RestoreFromSnapshot" => "Restored snapshot",
        "UnapplyBranch" => "Unapplied branch",
        "DeleteBranch" => "Deleted branch",
        "EnterEditMode" => "Entered edit mode",
        "ResolveConflicts" => "Resolved conflicts",
        "OnDemandSnapshot" => "Created snapshot",
        _ => "Snapshot",
    }
}

fn hook_report(value: &impl serde::Serialize) -> Result<HookReport> {
    let json = serde_json::to_value(value).map_err(|error| {
        BridgeError::internal(format!("Could not read the hook result: {error}"))
    })?;
    let status_name = json.get("status").and_then(|status| status.as_str());
    let status = match status_name {
        Some("success") => HookStatus::Success,
        Some("notconfigured") => HookStatus::NotConfigured,
        Some("failure") => HookStatus::Failed {
            error: json_string(&json, "error"),
        },
        Some("message") => HookStatus::Rewritten {
            message: json_string(&json, "message"),
        },
        Some(other) => HookStatus::Failed {
            error: format!("GitButler returned an unknown hook status ({other})."),
        },
        None => HookStatus::Failed {
            error: "GitButler returned a hook result with no status.".to_string(),
        },
    };
    Ok(HookReport { status })
}

fn json_string(json: &serde_json::Value, field: &str) -> String {
    json.get(field)
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string()
}

fn require_selection(selections: &[SelectionSpec]) -> Result<()> {
    if selections.is_empty() {
        Err(BridgeError::not_ready("Select a file or a hunk first."))
    } else {
        Ok(())
    }
}

fn require_message(message: &str) -> Result<()> {
    if message.trim().is_empty() {
        Err(BridgeError::not_ready("Write a commit message first."))
    } else {
        Ok(())
    }
}

pub(crate) fn parse_sha(sha: &str) -> Result<gix::ObjectId> {
    gix::ObjectId::from_hex(sha.as_bytes())
        .map_err(|_| BridgeError::not_found(format!("'{sha}' is not a commit id")))
}

fn not_exclusive() -> BridgeError {
    BridgeError::not_exclusive(NOT_EXCLUSIVE_MESSAGE).with_hint(NOT_EXCLUSIVE_HINT)
}

fn is_not_exclusive(error: &anyhow::Error) -> bool {
    let chain = format!("{error:#}");
    error.to_string().contains("already opened for writing")
        || chain.contains("already opened for writing")
}

pub(crate) fn api_error(action: &str, error: anyhow::Error) -> BridgeError {
    let chain = format!("{error:#}");
    let top = error.to_string();
    if is_not_exclusive(&error) {
        return not_exclusive();
    }
    let debug = format!("{error:?}");
    if chain.to_ascii_lowercase().contains("precondition")
        || debug.contains("PreconditionFailed")
        || top.contains("no commits were provided")
        || chain.contains("no commits were provided")
    {
        return BridgeError::not_ready(top).with_hint(chain);
    }
    BridgeError::internal(format!("Could not {action}: {top}")).with_hint(chain)
}

#[cfg(test)]
mod tests {
    use super::api_error;
    use crate::BridgeErrorKind;

    #[test]
    fn already_opened_maps_to_not_exclusive() {
        let error = anyhow::anyhow!(
            "Project at '/tmp/work' is already opened for writing by another GitButler instance"
        );
        let mapped = api_error("create a commit", error);
        assert_eq!(mapped.kind, BridgeErrorKind::NotExclusive);
        assert_eq!(
            mapped.message,
            "Another GitButler window has this project open."
        );
        assert_eq!(
            mapped.hint.as_deref(),
            Some("Close the other GitButler window, then try again.")
        );
    }

    #[test]
    fn a_precondition_stays_not_ready_and_keeps_the_chain() {
        let error = anyhow::anyhow!("precondition failed: no commits were provided");
        let mapped = api_error("squash these commits", error);
        assert_eq!(mapped.kind, BridgeErrorKind::NotReady);
        assert!(mapped.hint.as_deref().unwrap_or("").contains("no commits"));
    }
}
