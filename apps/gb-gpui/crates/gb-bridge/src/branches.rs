//! Branch list, details, apply, unapply, and delete.
//!
//! The list is one ref walk and does not talk to the network. Mutations go
//! through [`WriteGate`](crate::mutate::WriteGate). This module does not hold
//! the worktree write lock across those calls: each API takes it itself.

use but_ctx::Context;
use but_workspace::branch::apply::OutcomeStatus;

use crate::mutate::{self, WriteGate};
use crate::process::Hub;
use crate::read::{self, VersionSource};
use crate::{
    ApplyResult, BranchDetails, BranchPlace, BranchRow, BridgeError, CommitSummary, Result,
    WorkspaceSummary, branch_label, first_line, lossy_bytes, short_sha,
};

const BEHIND_CAP: usize = 20_000;

pub(crate) fn list_branches(source: VersionSource, id: &str) -> Result<Vec<BranchRow>> {
    let ctx = crate::live::open_context(source, id)?;
    rows_from(&ctx)
}

pub(crate) fn branch_details(
    source: VersionSource,
    id: &str,
    ref_name: &str,
) -> Result<BranchDetails> {
    let ctx = crate::live::open_context(source, id)?;
    let full = branch_ref(ref_name)?;
    let row = rows_from(&ctx)?
        .into_iter()
        .find(|row| row.ref_name == full)
        .ok_or_else(|| BridgeError::not_found(format!("No branch {ref_name}")))?;
    let commits = match commits_of(&ctx, &row) {
        Ok(commits) => commits,
        Err(_) if row.applied => lane_commits(source, id, &row.name)?,
        Err(error) => return Err(error),
    };
    Ok(BranchDetails {
        name: row.name,
        ref_name: row.ref_name,
        applied: row.applied,
        commits,
        ahead: row.ahead,
        behind: row.behind,
    })
}

pub(crate) fn apply_branch(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    ref_name: &str,
) -> Result<ApplyResult> {
    let full = branch_ref(ref_name)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let outcome = but_api::branch::apply(gate.ctx(), full.as_ref())
        .map_err(|error| mutate::api_error("apply this branch", error))?;
    let (names, already_applied) = match outcome.status {
        OutcomeStatus::ConflictAborted => {
            return Err(BridgeError::not_ready(
                "Applying this branch conflicts with the workspace.",
            ));
        }
        OutcomeStatus::AlreadyApplied => (Vec::new(), true),
        OutcomeStatus::Applied => (
            outcome
                .applied_branches
                .iter()
                .map(|name| branch_label(name.as_ref()))
                .collect(),
            false,
        ),
    };
    drop(gate);
    Ok(ApplyResult {
        workspace: read::open_project(source, id)?,
        names,
        already_applied,
    })
}

pub(crate) fn unapply_branch(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    name: &str,
) -> Result<WorkspaceSummary> {
    let short = short_branch_name(name)?;
    let mut gate = WriteGate::open(source, id, hub)?;
    let stack_id = mutate::stack_for_lane(gate.ctx(), &short)?;
    but_api::legacy::virtual_branches::unapply_stack(gate.ctx(), stack_id)
        .map_err(|error| mutate::api_error("unapply this branch", error))?;
    drop(gate);
    read::open_project(source, id)
}

pub(crate) fn delete_local_branch(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    name: &str,
) -> Result<WorkspaceSummary> {
    let full = branch_ref(name)?;
    if full.category() != Some(gix::refs::Category::LocalBranch) {
        return Err(BridgeError::not_ready(
            "Only a local branch can be deleted.",
        ));
    }
    let short = branch_label(full.as_ref());
    let mut gate = WriteGate::open(source, id, hub)?;
    let applied = {
        let (_guard, _repo, workspace, _db) = gate
            .ctx()
            .workspace_and_db()
            .map_err(|error| mutate::api_error("read the workspace", error))?;
        workspace
            .find_segment_and_stack_by_refname(full.as_ref())
            .is_some()
    };
    if applied {
        return Err(BridgeError::not_ready(format!(
            "Unapply {short} before deleting it. It is in the workspace."
        )));
    }
    but_api::branch::branch_remove(gate.ctx(), full)
        .map_err(|error| mutate::api_error("delete this branch", error))?;
    drop(gate);
    read::open_project(source, id)
}

fn rows_from(ctx: &Context) -> Result<Vec<BranchRow>> {
    let listed = but_api::branch::branch_list(ctx)
        .map_err(|error| mutate::api_error("list branches", error))?;
    let target = target_tip(ctx);
    let repo = ctx
        .repo
        .get()
        .map_err(|error| mutate::api_error("open the repository", error))?;
    let mut rows = Vec::new();
    for stack in listed {
        let stack_name = stack_label(&stack.status);
        let applied = stack_name == "applied";
        let stack_branches: Vec<String> = stack
            .branches
            .iter()
            .map(|row| display_name(&row.branch.display_name, &row.branch.ref_name))
            .collect();
        for row in &stack.branches {
            let branch = &row.branch;
            let name = display_name(&branch.display_name, &branch.ref_name);
            let place = match (branch.has_local, branch.remote_refs.is_empty()) {
                (true, true) => BranchPlace::Local,
                (false, false) => BranchPlace::Remote,
                (true, false) => BranchPlace::Both,
                (false, true) => BranchPlace::Local,
            };
            let behind = target.and_then(|target| commits_behind(&repo, branch.tip, target));
            rows.push(BranchRow {
                ref_name: branch.ref_name.to_string(),
                place,
                remotes: branch
                    .remote_refs
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
                applied,
                stack: stack_name.to_string(),
                stack_branches: stack_branches.clone(),
                tip_sha: branch.tip.to_string(),
                tip_short: short_sha(branch.tip),
                last_commit: commit_title(&repo, branch.tip),
                author: branch
                    .last_author
                    .as_ref()
                    .map(read::person_label)
                    .unwrap_or_default(),
                updated_at_ms: branch.updated_at_ms.or(stack.updated_at_ms),
                ahead: branch.commits_ahead_of_target,
                behind,
                name,
            });
        }
    }
    Ok(rows)
}

fn display_name(name: impl AsRef<[u8]>, ref_name: &gix::refs::FullName) -> String {
    let text = lossy_bytes(name);
    if text.is_empty() {
        branch_label(ref_name.as_ref())
    } else {
        text
    }
}

fn commits_of(ctx: &Context, row: &BranchRow) -> Result<Vec<CommitSummary>> {
    let remote = if row.place == BranchPlace::Remote {
        Some(remote_name(&row.ref_name, &row.name)?)
    } else {
        None
    };
    match but_api::legacy::workspace::branch_details(ctx, row.name.clone(), remote) {
        Ok(details) => Ok(details.commits.into_iter().map(commit_summary).collect()),
        // A missing local cache table can fail this read. The commits are still on the ref.
        Err(error) => match commits_on_ref(ctx, &row.ref_name) {
            Ok(commits) => Ok(commits),
            Err(_) => Err(mutate::api_error("read this branch", error)),
        },
    }
}

fn commits_on_ref(ctx: &Context, ref_name: &str) -> Result<Vec<CommitSummary>> {
    let repo = ctx
        .repo
        .get()
        .map_err(|error| mutate::api_error("open the repository", error))?;
    let mut reference = repo
        .find_reference(ref_name)
        .map_err(|error| mutate::api_error("read this branch", error.into()))?;
    let tip = reference
        .peel_to_commit()
        .map_err(|error| mutate::api_error("read this branch", error.into()))?
        .id;
    let hidden = target_tip(ctx);
    let platform = repo.rev_walk([tip]);
    let walk = match hidden {
        Some(target) => platform.with_hidden([target]).all(),
        None => platform.all(),
    }
    .map_err(|error| mutate::api_error("read this branch", error.into()))?;
    let mut commits = Vec::new();
    for item in walk {
        let id = item
            .map_err(|error| mutate::api_error("read this branch", error.into()))?
            .id;
        commits.push(commit_from_repo(&repo, id));
        if commits.len() >= 100 {
            break;
        }
    }
    Ok(commits)
}

fn commit_from_repo(repo: &gix::Repository, id: gix::ObjectId) -> CommitSummary {
    let Ok(commit) = repo.find_commit(id) else {
        return CommitSummary {
            sha: id.to_string(),
            short_sha: short_sha(id),
            title: String::new(),
            author: String::new(),
            authored_at: 0,
            conflicted: false,
            pushed: false,
        };
    };
    let title = commit
        .message()
        .ok()
        .map(|message| first_line(message.title))
        .unwrap_or_default();
    let (author, authored_at) = commit
        .author()
        .ok()
        .map(|signature| {
            let authored_at = signature.seconds();
            (actor_label(signature), authored_at)
        })
        .unwrap_or_default();
    CommitSummary {
        sha: id.to_string(),
        short_sha: short_sha(id),
        title,
        author,
        authored_at,
        conflicted: false,
        pushed: false,
    }
}

fn actor_label(signature: gix::actor::SignatureRef<'_>) -> String {
    let name = lossy_bytes(signature.name);
    let email = lossy_bytes(signature.email);
    if email.is_empty() {
        name
    } else {
        format!("{name} <{email}>")
    }
}

fn commit_summary(commit: but_workspace::ui::Commit) -> CommitSummary {
    let pushed = matches!(commit.state, but_core::ui::CommitState::LocalAndRemote(_));
    CommitSummary {
        sha: commit.id.to_string(),
        short_sha: short_sha(commit.id),
        title: first_line(&commit.message),
        author: author_label(&commit.author),
        authored_at: i64::try_from(commit.authored_at / 1000).unwrap_or(0),
        conflicted: commit.has_conflicts,
        pushed,
    }
}

fn author_label(author: &but_workspace::ui::Author) -> String {
    if author.email.is_empty() {
        author.name.clone()
    } else {
        format!("{} <{}>", author.name, author.email)
    }
}

fn lane_commits(source: VersionSource, id: &str, name: &str) -> Result<Vec<CommitSummary>> {
    let workspace = read::open_project(source, id)?;
    workspace
        .stacks
        .into_iter()
        .find_map(|stack| {
            stack
                .branches
                .into_iter()
                .find(|branch| branch.name == name)
                .map(|branch| branch.commits)
        })
        .ok_or_else(|| BridgeError::not_found(format!("No lane named {name}")))
}

fn target_tip(ctx: &Context) -> Option<gix::ObjectId> {
    let name = ctx.project_meta().ok()?.target_ref?;
    let repo = ctx.repo.get().ok()?;
    let mut reference = repo.find_reference(&name).ok()?;
    Some(reference.peel_to_commit().ok()?.id)
}

fn commits_behind(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    target: gix::ObjectId,
) -> Option<usize> {
    let walk = repo.rev_walk([target]).with_hidden([tip]).all().ok()?;
    let mut count = 0usize;
    for item in walk {
        item.ok()?;
        count += 1;
        if count > BEHIND_CAP {
            return None;
        }
    }
    Some(count)
}

fn commit_title(repo: &gix::Repository, id: gix::ObjectId) -> String {
    let Ok(commit) = repo.find_commit(id) else {
        return String::new();
    };
    let Ok(message) = commit.message() else {
        return String::new();
    };
    first_line(message.title)
}

fn stack_label(status: &impl std::fmt::Debug) -> &'static str {
    match format!("{status:?}").as_str() {
        "Applied" => "applied",
        "Unapplied" => "unapplied",
        "Target" => "target",
        _ => "standalone",
    }
}

fn remote_name(full: &str, display: &str) -> Result<String> {
    let rest = full.strip_prefix("refs/remotes/").ok_or_else(|| {
        BridgeError::not_ready(format!("{full} is not a remote-tracking branch."))
    })?;
    let suffix = format!("/{display}");
    rest.strip_suffix(&suffix)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| BridgeError::not_ready(format!("{full} is not a remote-tracking branch.")))
}

pub(crate) fn branch_ref(name: &str) -> Result<gix::refs::FullName> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(BridgeError::not_ready("Choose a branch."));
    }
    if trimmed.starts_with("refs/") {
        return gix::refs::FullName::try_from(trimmed).map_err(|error| {
            BridgeError::not_ready(format!("'{trimmed}' is not a branch name: {error}"))
        });
    }
    mutate::local_ref(trimmed)
}

pub(crate) fn short_branch_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(BridgeError::not_ready("Choose a branch."));
    }
    if let Some(rest) = trimmed.strip_prefix("refs/heads/") {
        if rest.is_empty() {
            return Err(BridgeError::not_ready(format!(
                "'{trimmed}' is not a branch name"
            )));
        }
        return Ok(rest.to_string());
    }
    if trimmed.starts_with("refs/") {
        return Err(BridgeError::not_ready(format!(
            "'{trimmed}' is not a local branch."
        )));
    }
    Ok(trimmed.to_string())
}
