//! Project setup, fetch, upstream integrate, and push.
//!
//! Each write opens a [`WriteGate`](crate::mutate::WriteGate) so it takes
//! `project.lock` the same way other mutations do. The GitButler calls below
//! take `exclusive_worktree_access` themselves. This module does not hold that
//! lock across them. The base-branch read takes it only for that read, after
//! the write has returned.
//!
//! The desktop push command also updates review state after the push. This
//! bridge calls the git-only push. Review sync stays out of this slice.

use but_ctx::Context;

use crate::mutate::{self, WriteGate};
use crate::process::Hub;
use crate::read::{self, VersionSource};
use crate::{
    BaseBranchData, BridgeError, FetchReport, FetchStatus, ForcePush, IntegratePreview,
    IntegrateReport, IntegrateResolution, LaneResolution, ProjectSummary, PushReport, PushedBranch,
    RemoteBranch, Result, UpstreamCommit, UpstreamStrategy, WorkspaceSummary, first_line,
    lossy_bytes,
};

const PROJECTS_QUEUE: &str = "__projects__";

pub(crate) fn projects_queue_key() -> &'static str {
    PROJECTS_QUEUE
}

pub(crate) fn add_project(source: VersionSource, path: &str) -> Result<ProjectSummary> {
    read::prepared_to_add(source)?;
    if path.trim().is_empty() {
        return Err(BridgeError::not_ready("Choose a repository folder."));
    }
    let outcome = but_api::legacy::projects::add_project(path.to_string())
        .map_err(|error| mutate::api_error("add this repository", error))?;
    let project = match outcome {
        gitbutler_project::AddProjectOutcome::Added(project)
        | gitbutler_project::AddProjectOutcome::AlreadyExists(project) => project,
        gitbutler_project::AddProjectOutcome::PathNotFound => {
            return Err(BridgeError::not_ready("That folder does not exist."));
        }
        gitbutler_project::AddProjectOutcome::NotADirectory => {
            return Err(BridgeError::not_ready("That path is not a folder."));
        }
        gitbutler_project::AddProjectOutcome::BareRepository => {
            return Err(BridgeError::not_ready(
                "That repository has no worktree. Add the checkout, not the bare repository.",
            ));
        }
        gitbutler_project::AddProjectOutcome::NonMainWorktree => {
            return Err(BridgeError::not_ready(
                "Add the main worktree. This path is a linked worktree.",
            ));
        }
        gitbutler_project::AddProjectOutcome::NoWorkdir => {
            return Err(BridgeError::not_ready(
                "That repository has no working directory.",
            ));
        }
        gitbutler_project::AddProjectOutcome::NoDotGitDirectory => {
            return Err(BridgeError::not_ready(
                "That folder is not a git repository.",
            ));
        }
        gitbutler_project::AddProjectOutcome::ReftableRefFormatUnsupported => {
            return Err(BridgeError::not_ready(
                "This repository uses the reftable format, which GitButler cannot open.",
            ));
        }
        gitbutler_project::AddProjectOutcome::NotAGitRepository(_) => {
            return Err(BridgeError::not_ready(
                "That folder is not a git repository.",
            ));
        }
    };
    let summary = read::project_summary(project)?;
    // Opening the project creates its database the way GitButler does the first
    // time the project is opened. Settings were already on disk.
    let handle: but_ctx::ProjectHandleOrLegacyProjectId = summary.id.parse().map_err(|error| {
        BridgeError::internal(format!("The new project id is not valid: {error}"))
    })?;
    let ctx = Context::try_from(handle)
        .map_err(|error| mutate::api_error("open the new project", error))?;
    let _database = ctx
        .db
        .get_cache()
        .map_err(|error| mutate::api_error("prepare this project's database", error))?;
    Ok(summary)
}

pub(crate) fn delete_project(source: VersionSource, id: &str) -> Result<()> {
    read::prepared_project(source, id)?;
    let handle = id.parse().map_err(|error| {
        BridgeError::not_found(format!("Project id '{id}' is not valid: {error}"))
    })?;
    but_api::legacy::projects::delete_project(handle)
        .map_err(|error| mutate::api_error("remove this project", error))?;
    Ok(())
}

pub(crate) fn remote_branches(source: VersionSource, id: &str) -> Result<Vec<RemoteBranch>> {
    let ctx = crate::live::open_context(source, id)?;
    let refs = but_api::legacy::git::git_remote_branches(&ctx)
        .map_err(|error| mutate::api_error("list remote branches", error))?;
    let mut branches: Vec<_> = refs
        .into_iter()
        .map(|name| RemoteBranch {
            remote: name.remote().to_string(),
            branch: name.branch().to_string(),
            full_name: name.to_string(),
        })
        .collect();
    branches.sort_by(|left, right| left.full_name.cmp(&right.full_name));
    Ok(branches)
}

pub(crate) fn base_branch(source: VersionSource, id: &str) -> Result<Option<BaseBranchData>> {
    let mut ctx = crate::live::open_context(source, id)?;
    let mut guard = ctx.exclusive_worktree_access();
    let data =
        but_api::legacy::virtual_branches::get_base_branch_data(&ctx, guard.write_permission())
            .map_err(|error| mutate::api_error("read the base branch", error))?;
    drop(guard);
    Ok(data.map(|base| {
        let upstream_commits = base
            .upstream_commits
            .iter()
            .map(|commit| UpstreamCommit {
                short_sha: short_hex(&commit.id),
                title: first_line(&commit.description),
                author: format!("{} <{}>", commit.author.name, commit.author.email),
                sha: commit.id.clone(),
            })
            .collect();
        BaseBranchData {
            branch_name: base.branch_name,
            short_name: base.short_name,
            remote_name: base.remote_name,
            remote_url: base.remote_url,
            push_remote_name: base.push_remote_name,
            push_remote_url: base.push_remote_url,
            base_sha: base.base_sha.to_string(),
            current_sha: base.current_sha.to_string(),
            behind: base.behind,
            upstream_commits,
            last_fetched_ms: base.last_fetched_ms,
            conflicted: base.conflicted,
        }
    }))
}

pub(crate) fn set_target(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    target_ref: &str,
    push_remote: Option<&str>,
) -> Result<BaseBranchData> {
    {
        let mut gate = WriteGate::open(source, id, hub)?;
        let target = gix::refs::FullName::try_from(target_ref).map_err(|error| {
            BridgeError::not_ready(format!("'{target_ref}' is not a branch ref: {error}"))
        })?;
        let push_remote = push_remote
            .map(str::trim)
            .filter(|remote| !remote.is_empty())
            .map(str::to_string);
        but_api::workspace::set_target_ref_and_init_project(
            gate.ctx(),
            target.as_ref(),
            push_remote,
        )
        .map_err(|error| mutate::api_error("set the target branch", error))?;
    }
    base_branch(source, id)?.ok_or_else(|| {
        BridgeError::not_ready("The target was saved, but GitButler has no base branch data yet.")
    })
}

pub(crate) fn fetch(source: VersionSource, id: &str, hub: &Hub) -> Result<FetchReport> {
    {
        let mut gate = WriteGate::open(source, id, hub)?;
        but_api::workspace::workspace_fetch_from_remotes(gate.ctx(), Some("fetch".to_string()))
            .map_err(fetch_error)?;
    }
    let status = fetch_status(source, id)?;
    let summary = status.summary.clone();
    Ok(FetchReport {
        status,
        steps: vec!["Fetched remotes".to_string(), summary],
    })
}

pub(crate) fn fetch_status(source: VersionSource, id: &str) -> Result<FetchStatus> {
    let ctx = crate::live::open_context(source, id)?;
    let recorded = but_api::workspace::workspace_fetch_status(&ctx)
        .map_err(|error| mutate::api_error("read fetch status", error))?;
    let base = base_branch(source, id)?;
    let (upstream_count, commits) = match base {
        Some(base) => (base.behind, base.upstream_commits),
        None => (0, Vec::new()),
    };
    Ok(FetchStatus {
        summary: FetchStatus::summary_for(upstream_count),
        upstream_count,
        last_fetched_ms: recorded.last_successful_ms.map(u128::from),
        last_attempted_ms: recorded.last_attempted_ms.map(u128::from),
        last_error: recorded.last_error,
        commits,
    })
}

pub(crate) fn integrate(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    strategy: UpstreamStrategy,
    dry_run: bool,
) -> Result<(WorkspaceSummary, Vec<LaneResolution>, Vec<String>)> {
    let before = read::open_project(source, id)?;
    let updates = stack_updates(&before, strategy)?;
    let outcome = {
        let mut gate = WriteGate::open(source, id, hub)?;
        but_api::workspace::workspace_integrate_upstream(
            gate.ctx(),
            updates,
            if dry_run {
                but_core::DryRun::Yes
            } else {
                but_core::DryRun::No
            },
        )
        .map_err(|error| mutate::api_error("integrate upstream commits", error))?
    };
    let after = read::summarize(id, &outcome.workspace_state.head_info);
    let resolutions = compare_lanes(&before, &after, strategy);
    let worktree_conflicts = outcome.worktree_conflicts.iter().map(lossy_bytes).collect();
    Ok((after, resolutions, worktree_conflicts))
}

pub(crate) fn preview(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    strategy: UpstreamStrategy,
) -> Result<IntegratePreview> {
    let (_workspace, resolutions, worktree_conflicts) = integrate(source, id, hub, strategy, true)?;
    let summary = preview_summary(&resolutions);
    Ok(IntegratePreview {
        resolutions,
        worktree_conflicts,
        summary,
    })
}

pub(crate) fn apply(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    strategy: UpstreamStrategy,
) -> Result<IntegrateReport> {
    let (workspace, resolutions, worktree_conflicts) = integrate(source, id, hub, strategy, false)?;
    Ok(IntegrateReport {
        workspace,
        resolutions,
        worktree_conflicts,
    })
}

pub(crate) fn push(
    source: VersionSource,
    id: &str,
    hub: &Hub,
    lane: &str,
    force: ForcePush,
) -> Result<PushReport> {
    let reference = mutate::local_ref(lane)?;
    let (with_force, skip_protection) = match force {
        ForcePush::No => (false, false),
        ForcePush::WithLease => (true, false),
        ForcePush::Force => (true, true),
    };
    let mut gate = WriteGate::open(source, id, hub)?;
    let result = but_api::legacy::workspace::workspace_branch_and_ancestors_push_only(
        gate.ctx(),
        with_force,
        skip_protection,
        reference.as_ref(),
        true,
        Vec::new(),
    )
    .map_err(push_error)?;
    drop(gate);
    let sha_updates = result.branch_sha_updates.clone();
    let branches = result
        .branch_to_remote
        .into_iter()
        .map(|(name, remote_ref, remote_branch)| {
            let (before_sha, after_sha) = sha_updates
                .iter()
                .find(|(branch, _, _)| branch == &name)
                .map(|(_, before, after)| (before.clone(), after.clone()))
                .unwrap_or_default();
            PushedBranch {
                name,
                remote_ref: remote_ref.to_string(),
                remote_branch,
                before_sha,
                after_sha,
            }
        })
        .collect::<Vec<_>>();
    let summary = if branches.is_empty() {
        "Nothing to push".to_string()
    } else {
        let names = branches
            .iter()
            .map(|branch| branch.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("Pushed {names}")
    };
    Ok(PushReport {
        remote: result.remote,
        steps: vec![summary.clone()],
        summary,
        force,
        branches,
    })
}

fn stack_updates(
    workspace: &WorkspaceSummary,
    strategy: UpstreamStrategy,
) -> Result<Vec<but_api::workspace::json::BottomUpdate>> {
    let kind = match strategy {
        UpstreamStrategy::Rebase => but_api::workspace::json::BottomUpdateKind::Rebase,
        UpstreamStrategy::Merge => but_api::workspace::json::BottomUpdateKind::Merge,
    };
    let mut updates = Vec::new();
    for stack in &workspace.stacks {
        let Some(bottom) = stack.branches.last() else {
            continue;
        };
        let selector = if let Some(commit) = bottom.commits.last() {
            but_api::commit::json::RelativeTo::Commit(mutate::parse_sha(&commit.sha)?)
        } else if !bottom.name.is_empty() {
            but_api::commit::json::RelativeTo::Reference(mutate::local_ref(&bottom.name)?)
        } else {
            continue;
        };
        updates.push(but_api::workspace::json::BottomUpdate { kind, selector });
    }
    Ok(updates)
}

fn compare_lanes(
    before: &WorkspaceSummary,
    after: &WorkspaceSummary,
    strategy: UpstreamStrategy,
) -> Vec<LaneResolution> {
    lane_names(before)
        .into_iter()
        .map(|lane| {
            let present = lane_names(after).into_iter().any(|name| name == lane);
            let resolution = if !present {
                IntegrateResolution::Integrated
            } else if lane_conflicted(after, &lane) {
                IntegrateResolution::Conflicted
            } else {
                IntegrateResolution::Clear
            };
            let commits = if present {
                lane_titles(after, &lane)
            } else {
                Vec::new()
            };
            LaneResolution {
                lane,
                strategy,
                resolution,
                commits,
            }
        })
        .collect()
}

fn lane_names(workspace: &WorkspaceSummary) -> Vec<String> {
    let mut names = Vec::new();
    for stack in &workspace.stacks {
        for branch in &stack.branches {
            if !names.contains(&branch.name) {
                names.push(branch.name.clone());
            }
        }
    }
    names
}

fn lane_titles(workspace: &WorkspaceSummary, name: &str) -> Vec<String> {
    workspace
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == name)
        .map(|branch| {
            branch
                .commits
                .iter()
                .map(|commit| commit.title.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn lane_conflicted(workspace: &WorkspaceSummary, name: &str) -> bool {
    workspace
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == name)
        .is_some_and(|branch| branch.commits.iter().any(|commit| commit.conflicted))
}

fn preview_summary(resolutions: &[LaneResolution]) -> String {
    let Some(first) = resolutions.first() else {
        return "No lanes to integrate.".to_string();
    };
    let count = resolutions.len();
    let lanes = if count == 1 { "lane" } else { "lanes" };
    match first.strategy {
        UpstreamStrategy::Rebase => format!("{count} {lanes} will rebase onto the upstream."),
        UpstreamStrategy::Merge => format!("{count} {lanes} will merge the upstream."),
    }
}

fn fetch_error(error: anyhow::Error) -> BridgeError {
    let blob = error_blob(&error);
    if blob.contains("askpassexecutablenotfound") || blob.contains("gitbutler-git-askpass") {
        return BridgeError::not_ready(
            "Git asked for a password, and the askpass helper is not installed.",
        )
        .with_hint("Set GITBUTLER_ASKPASS_BIN to the askpass helper, then try again.");
    }
    mutate::api_error("fetch from remotes", error)
}

fn push_error(error: anyhow::Error) -> BridgeError {
    let blob = error_blob(&error);
    if blob.contains("askpassexecutablenotfound") || blob.contains("gitbutler-git-askpass") {
        return BridgeError::not_ready(
            "Git asked for a password, and the askpass helper is not installed.",
        )
        .with_hint("Set GITBUTLER_ASKPASS_BIN to the askpass helper, then try again.");
    }
    if is_force_protection(&blob) {
        return BridgeError::not_ready(
            "The push was rejected. The remote has commits that a protected force push would overwrite.",
        )
        .with_hint("Fetch, then integrate, or confirm an unprotected force push.");
    }
    if is_non_fast_forward(&blob) {
        return BridgeError::not_ready(
            "The push was rejected. The remote has commits this lane does not.",
        )
        .with_hint("Fetch and integrate the upstream commits, or confirm a force push.");
    }
    mutate::api_error("push this lane", error)
}

fn error_blob(error: &anyhow::Error) -> String {
    format!("{error:#}\n{error:?}").to_ascii_lowercase()
}

fn is_force_protection(blob: &str) -> bool {
    blob.contains("forcepushprotection")
        || blob.contains("gitforcepushprotection")
        || blob.contains("force push was blocked")
        || blob.contains("force-with-lease")
        || blob.contains("would be overwritten")
}

fn is_non_fast_forward(blob: &str) -> bool {
    blob.contains("nonfastforward")
        || blob.contains("non-fast-forward")
        || blob.contains("gitnonfastforward")
        || blob.contains("[rejected]")
        || blob.contains("failed to push")
        || blob.contains("rejected")
}

fn short_hex(sha: &str) -> String {
    sha.chars().take(7).collect()
}
