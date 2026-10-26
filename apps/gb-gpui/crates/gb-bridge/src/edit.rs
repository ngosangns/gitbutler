//! Edit mode and conflict resolution.
//!
//! Other writes refuse while a commit is checked out for editing. Save and
//! abort are the exceptions: they use [`WriteGate::open_during_edit`].
//! Entering edit mode looks up the stack from the commit. Callers do not
//! pass a stack id.

use but_ctx::Context;

use crate::mutate::{self, WriteGate};
use crate::process::Hub;
use crate::read::{self, VersionSource};
use crate::{
    BridgeError, BridgeEvent, ConflictChoice, ConflictedCommit, EDIT_MODE_MESSAGE, EditStatus,
    FileChoice, ResolveReport, Result, WorkspaceSummary, lossy_bytes,
};

struct ModeView {
    head: Option<String>,
    mode: String,
    commit_sha: Option<String>,
    stack_id: Option<String>,
}

pub(crate) fn refuse_if_editing(ctx: &Context) -> Result<()> {
    if is_editing(ctx) {
        Err(BridgeError::not_ready(EDIT_MODE_MESSAGE))
    } else {
        Ok(())
    }
}

pub(crate) fn edit_status(source: VersionSource, id: &str) -> Result<EditStatus> {
    let ctx = crate::live::open_context(source, id)?;
    status_of(&ctx)
}

pub(crate) fn enter_edit_mode(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    sha: &str,
) -> Result<EditStatus> {
    let oid = mutate::parse_sha(sha)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let stack_id = {
        let (_guard, _repo, workspace, _db) = gate
            .ctx()
            .workspace_and_db()
            .map_err(|error| mutate::api_error("read the workspace", error))?;
        let (stack, _, _) = workspace.find_commit_and_containers(oid).ok_or_else(|| {
            BridgeError::not_found(format!("Commit {sha} is not in this workspace."))
        })?;
        stack
            .id
            .ok_or_else(|| BridgeError::not_found("That commit is not on a stack."))?
    };
    but_api::legacy::modes::enter_edit_mode(gate.ctx(), oid, stack_id)
        .map_err(|error| mutate::api_error("enter edit mode", error))?;
    let status = status_of(gate.ctx())?;
    emit_mode(id, hub, &status);
    Ok(status)
}

pub(crate) fn save_edit(source: VersionSource, id: &str, hub: &Hub) -> Result<WorkspaceSummary> {
    let mut gate = WriteGate::open_during_edit(source, id, hub)?;
    if !is_editing(gate.ctx()) {
        return Err(BridgeError::not_ready("This project is not in edit mode."));
    }
    but_api::legacy::modes::save_edit_and_return_to_workspace(gate.ctx())
        .map_err(|error| mutate::api_error("save this edit", error))?;
    emit_from_ctx(gate.ctx(), id, hub);
    drop(gate);
    read::open_project(source, id)
}

pub(crate) fn abort_edit(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    force: bool,
) -> Result<WorkspaceSummary> {
    let mut gate = WriteGate::open_during_edit(source, id, hub)?;
    if !is_editing(gate.ctx()) {
        return Err(BridgeError::not_ready("This project is not in edit mode."));
    }
    if let Err(error) =
        but_api::legacy::modes::abort_edit_and_return_to_workspace(gate.ctx(), force)
    {
        return Err(abort_error(error));
    }
    emit_from_ctx(gate.ctx(), id, hub);
    drop(gate);
    read::open_project(source, id)
}

pub(crate) fn conflicted_commits(source: VersionSource, id: &str) -> Result<Vec<ConflictedCommit>> {
    let workspace = read::open_project(source, id)?;
    let mut commits = Vec::new();
    for stack in workspace.stacks {
        for branch in stack.branches {
            for commit in branch.commits {
                if !commit.conflicted {
                    continue;
                }
                commits.push(ConflictedCommit {
                    sha: commit.sha,
                    short_sha: commit.short_sha,
                    title: commit.title,
                    author: commit.author,
                    lane: branch.name.clone(),
                });
            }
        }
    }
    Ok(commits)
}

pub(crate) fn resolve_conflicts(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    sha: &str,
    files: &[FileChoice],
) -> Result<ResolveReport> {
    if files.is_empty() {
        return Err(BridgeError::not_ready(
            "Choose ours or theirs for a conflicted file.",
        ));
    }
    let oid = mutate::parse_sha(sha)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let conflicts = but_api::resolve::commit_conflicts(gate.ctx(), oid)
        .map_err(|error| mutate::api_error("read this commit's conflicts", error))?;
    if conflicts.files.is_empty() && conflicts.manual.is_empty() {
        return Err(BridgeError::not_ready("That commit has no conflicts."));
    }
    let mut specs = Vec::new();
    for choice in files {
        if conflicts
            .manual
            .iter()
            .any(|manual| manual.path == choice.path)
        {
            return Err(BridgeError::not_ready(format!(
                "{} needs edit mode. This conflict has no hunk to choose.",
                choice.path
            )));
        }
        let Some(file) = conflicts.files.iter().find(|file| file.path == choice.path) else {
            return Err(BridgeError::not_found(format!(
                "No conflict in {}.",
                choice.path
            )));
        };
        if file.hunks.is_empty() {
            return Err(BridgeError::not_ready(format!(
                "{} needs edit mode. This conflict has no hunk to choose.",
                choice.path
            )));
        }
        let resolution = match choice.choice {
            ConflictChoice::Ours => but_api::resolve::HunkResolution::Ours,
            ConflictChoice::Theirs => but_api::resolve::HunkResolution::Theirs,
        };
        for index in 1..=file.hunks.len() {
            specs.push(but_api::resolve::ResolutionSpec {
                path: choice.path.clone(),
                hunk: index,
                resolution: resolution.clone(),
            });
        }
    }
    let resolved = but_api::resolve::resolve_commit_conflict_hunks(gate.ctx(), oid, specs)
        .map_err(|error| mutate::api_error("resolve these conflicts", error))?;
    let workspace = read::summarize(id, &resolved.workspace.head_info);
    Ok(ResolveReport {
        previous_sha: sha.to_string(),
        new_sha: resolved.new_commit.to_string(),
        resolved: resolved.resolved,
        still_conflicted: !resolved.remaining.is_empty() || !resolved.manual.is_empty(),
        workspace,
    })
}

fn is_editing(ctx: &Context) -> bool {
    match but_api::legacy::modes::operating_mode(ctx) {
        Ok(head) => view_of(&head).mode == "edit",
        Err(_) => false,
    }
}

fn status_of(ctx: &Context) -> Result<EditStatus> {
    let head = but_api::legacy::modes::operating_mode(ctx)
        .map_err(|error| BridgeError::internal(format!("Could not read edit mode: {error:?}")))?;
    let view = view_of(&head);
    let conflicted_files = if view.mode == "edit" {
        conflicted_paths(ctx)?
    } else {
        Vec::new()
    };
    Ok(EditStatus {
        editing: view.mode == "edit",
        commit_sha: view.commit_sha,
        stack_id: view.stack_id,
        head: view.head,
        conflicted_files,
    })
}

fn conflicted_paths(ctx: &Context) -> Result<Vec<String>> {
    let rows = but_api::legacy::modes::edit_initial_index_state(ctx)
        .map_err(|error| mutate::api_error("read edit conflicts", error))?;
    Ok(rows
        .into_iter()
        .filter_map(|(change, presence)| presence.map(|_| lossy_bytes(&change.path_bytes)))
        .collect())
}

fn emit_mode(id: &str, hub: &Hub, status: &EditStatus) {
    hub.emit(
        id,
        BridgeEvent::GitHead {
            head: status.head.clone(),
            mode: if status.editing {
                "edit".to_string()
            } else {
                "workspace".to_string()
            },
        },
    );
}

fn emit_from_ctx(ctx: &Context, id: &str, hub: &Hub) {
    if let Ok(head) = but_api::legacy::modes::operating_mode(ctx) {
        let view = view_of(&head);
        hub.emit(
            id,
            BridgeEvent::GitHead {
                head: view.head,
                mode: view.mode,
            },
        );
    }
}

fn view_of(head: &but_api::legacy::modes::HeadAndMode) -> ModeView {
    if let Ok(value) = serde_json::to_value(head) {
        let head_name = value
            .get("head")
            .and_then(|item| item.as_str())
            .map(str::to_string)
            .or_else(|| head.head.clone());
        if let Some(mode) = value.get("operatingMode")
            && let Some(view) = mode_from_value(head_name.clone(), mode)
        {
            return view;
        }
    }
    let rendered = format!("{head:?}");
    let mode = if rendered.contains("Edit(") || rendered.contains("Edit {") {
        "edit"
    } else if rendered.contains("OpenWorkspace") {
        "workspace"
    } else {
        "outside"
    };
    ModeView {
        head: head.head.clone(),
        mode: mode.to_string(),
        commit_sha: None,
        stack_id: None,
    }
}

fn mode_from_value(head: Option<String>, mode: &serde_json::Value) -> Option<ModeView> {
    let (name, subject) = match mode {
        serde_json::Value::String(text) => (text.as_str(), None),
        serde_json::Value::Object(map) => {
            let name = map.get("type")?.as_str()?;
            (name, map.get("subject"))
        }
        _ => return None,
    };
    let label = match name {
        "OpenWorkspace" => "workspace",
        "Edit" => "edit",
        _ => "outside",
    };
    let mut commit_sha = None;
    let mut stack_id = None;
    if label == "edit"
        && let Some(subject) = subject
    {
        commit_sha = subject
            .get("commitOid")
            .and_then(|item| item.as_str())
            .map(str::to_string);
        stack_id = subject
            .get("stackId")
            .and_then(|item| item.as_str())
            .map(str::to_string);
    }
    Some(ModeView {
        head,
        mode: label.to_string(),
        commit_sha,
        stack_id,
    })
}

fn abort_error(error: anyhow::Error) -> BridgeError {
    let text = format!("{error:#}");
    if text.contains("forced abort") || text.to_ascii_lowercase().contains("working tree differs") {
        return BridgeError::not_ready(
            "This edit has changes. Abort it only if you mean to drop them.",
        );
    }
    mutate::api_error("abort this edit", error)
}
