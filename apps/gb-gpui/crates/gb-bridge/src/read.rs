//! Read-only project and workspace access.
//!
//! `but_api::legacy::workspace::head_info` is not used. It builds a
//! `but_ctx::Context`, and that context opens the project database with
//! `DbHandle::new_in_directory` (WAL pragma, checkpoint, migrations) and loads
//! metadata through `BranchOrderMetadata::from_paths`. The "read-only" metadata
//! constructor still calls `read_synced_virtual_branches`, which takes an
//! immediate write transaction and commits it. Either path fails the no-write
//! requirement.
//!
//! This module instead:
//! 1. stats `projects.json` and `settings.json` and never creates them,
//! 2. opens `but.sqlite` with `SQLITE_OPEN_READ_ONLY`,
//! 3. runs `but_db::migration::run` on that connection (a pending migration
//!    tries to write and fails; a current database returns `Ok(0)` from a
//!    deferred read that is rolled back),
//! 4. reads `virtual_branches().get_snapshot()` (deferred SELECTs),
//! 5. calls `but_workspace::head_info` with in-memory metadata and
//!    `Options.worktrees = false` so linked-worktree adoption cannot write.

use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use but_core::RefMetadata as _;
use but_core::RepositoryExt as _;
use but_core::ref_metadata::{
    ProjectMeta, RefInfo as MetaRefInfo, StackId, Workspace, WorkspaceCommitRelation,
    WorkspaceStack, WorkspaceStackBranch,
};
use but_db::DbHandle;
use gitbutler_project::Project;

use crate::memory_meta::MemoryMeta;
use crate::{
    BranchSummary, BridgeError, CommitSummary, Compat, Environment, PINNED_VERSION, ProjectList,
    ProjectSummary, Result, StackSummary, WorkspaceSummary, branch_label, compat_error, first_line,
    lossy_bytes, short_sha,
};

const PROJECTS_FILE: &str = "projects.json";
const SETTINGS_FILE: &str = "settings.json";
const OPEN_IN_GITBUTLER: &str = "Open this project in GitButler first";

#[derive(Clone, Copy)]
pub enum VersionSource {
    Detect,
    /// `None` means "not installed".
    Override(Option<&'static str>),
}

fn channel_error(identifier: &str) -> Result<()> {
    if identifier == crate::RELEASE_IDENTIFIER {
        Ok(())
    } else {
        Err(BridgeError::compat(crate::WRONG_CHANNEL_MESSAGE))
    }
}

pub fn environment(source: VersionSource) -> Result<Environment> {
    // Identifier is chosen at compile time. A dev-channel build looks beside
    // the installed app's project list and reports it as missing.
    channel_error(but_path::identifier())?;
    let app_data_dir = but_path::app_data_dir().map_err(|error| {
        BridgeError::internal(format!(
            "Could not resolve GitButler's data directory: {error}"
        ))
    })?;
    let config_dir = but_path::app_config_dir().map_err(|error| {
        BridgeError::internal(format!(
            "Could not resolve GitButler's settings directory: {error}"
        ))
    })?;
    let installed = match source {
        VersionSource::Detect => installed_short_version(),
        VersionSource::Override(version) => version.map(str::to_string),
    };
    let compat = match &installed {
        None => Compat::NotInstalled,
        Some(version) if version == PINNED_VERSION => Compat::Match,
        Some(version) => Compat::Mismatch {
            installed: version.clone(),
            pinned: PINNED_VERSION.to_string(),
        },
    };
    Ok(Environment {
        installed,
        pinned: PINNED_VERSION.to_string(),
        app_data_dir,
        config_dir,
        compat,
    })
}

pub fn list_projects(source: VersionSource) -> Result<ProjectList> {
    let env = environment(source)?;
    if env.compat != Compat::Match {
        return Err(compat_error(&env));
    }
    if !env.app_data_dir.join(PROJECTS_FILE).is_file() {
        return Ok(ProjectList {
            projects: Vec::new(),
            reason: Some(
                "Open GitButler once so it can create its project list. gb-gpui will not create that file."
                    .to_string(),
            ),
        });
    }
    if !env.config_dir.join(SETTINGS_FILE).is_file() {
        return Ok(ProjectList {
            projects: Vec::new(),
            reason: Some(
                "Open GitButler once so it can create its settings. gb-gpui will not create that file."
                    .to_string(),
            ),
        });
    }
    let projects =
        gitbutler_project::dangerously_list_projects_without_migration().map_err(|error| {
            BridgeError::internal(format!("Could not read the project list: {error}"))
        })?;
    let mut summaries = Vec::new();
    for project in projects {
        let id = project.id.to_string();
        let title = project.title.clone();
        match project.migrated() {
            Ok(project) => match worktree_path(&project) {
                Ok(path) => summaries.push(ProjectSummary {
                    id,
                    title,
                    path: path.display().to_string(),
                }),
                Err(error) => {
                    eprintln!("gb-bridge warning: skip project {id}: {error}");
                }
            },
            Err(error) => {
                eprintln!("gb-bridge warning: skip project {id}: {error}");
            }
        }
    }
    Ok(ProjectList {
        projects: summaries,
        reason: None,
    })
}

pub fn open_project(source: VersionSource, id: &str) -> Result<WorkspaceSummary> {
    let env = environment(source)?;
    if env.compat != Compat::Match {
        return Err(compat_error(&env));
    }
    if !env.app_data_dir.join(PROJECTS_FILE).is_file()
        || !env.config_dir.join(SETTINGS_FILE).is_file()
    {
        return Err(BridgeError::not_ready(
            "Open GitButler once so it can create its project list and settings. gb-gpui will not create those files.",
        ));
    }
    let projects =
        gitbutler_project::dangerously_list_projects_without_migration().map_err(|error| {
            BridgeError::internal(format!("Could not read the project list: {error}"))
        })?;
    let project = projects
        .into_iter()
        .find(|project| project.id.to_string() == id)
        .ok_or_else(|| BridgeError::not_found(format!("No project with id {id}")))?;
    let project = project.migrated().map_err(|error| {
        BridgeError::not_ready(format!("Open this project in GitButler first ({error})"))
    })?;
    let worktree = worktree_path(&project)?;
    let storage = storage_dir(&worktree)?;
    ensure_migrations_current(&storage)?;
    let mut db = DbHandle::open_existing_read_only_in_directory(&storage)
        .map_err(|error| BridgeError::not_ready(format!("{OPEN_IN_GITBUTLER} ({error})")))?
        .ok_or_else(|| BridgeError::not_ready(OPEN_IN_GITBUTLER))?;
    let snapshot = db
        .virtual_branches()
        .get_snapshot()
        .map_err(|error| BridgeError::not_ready(format!("{OPEN_IN_GITBUTLER} ({error})")))?;
    let Some(snapshot) = snapshot else {
        return Err(BridgeError::not_ready(OPEN_IN_GITBUTLER));
    };
    let workspace = workspace_from_snapshot(&snapshot)?;
    let mut meta = MemoryMeta::default();
    let workspace_ref =
        gix::refs::FullName::try_from(but_core::WORKSPACE_REF_NAME).map_err(|error| {
            BridgeError::internal(format!("Workspace ref name is invalid: {error}"))
        })?;
    {
        let mut handle = meta.workspace(workspace_ref.as_ref()).map_err(|error| {
            BridgeError::internal(format!("Could not stage workspace metadata: {error}"))
        })?;
        *handle = workspace;
        meta.set_workspace(&handle).map_err(|error| {
            BridgeError::internal(format!("Could not stage workspace metadata: {error}"))
        })?;
    }

    let mut repo = gix::open_opts(&worktree, gix::open::Options::isolated()).map_err(|error| {
        BridgeError::internal(format!("Could not open {}: {error}", worktree.display()))
    })?;
    repo.object_cache_size_if_unset(64 * 1024 * 1024);
    // Object writes stay in memory. The graph read does not need to persist objects.
    let repo = repo.with_object_memory();
    let project_meta = ProjectMeta::resolve(&repo).map_err(|error| {
        BridgeError::internal(format!(
            "Could not read the project's target from git config: {error}"
        ))
    })?;

    let mut traversal = but_graph::init::Options::limited();
    // Discovery runs a one-time worktree adoption that writes the project database.
    traversal.worktrees = false;
    let options = but_workspace::ref_info::Options {
        project_meta,
        traversal,
        expensive_commit_info: true,
        gerrit_mode: but_workspace::ref_info::GerritMode::Disabled,
    };
    let info = but_workspace::head_info(&repo, &meta, &mut db, options)
        .map_err(|error| BridgeError::internal(format!("Could not read the workspace: {error}")))?;
    let info = info.pruned_to_entrypoint();
    Ok(summarize(id, &info))
}

/// `migration::run` records applied versions in `__diesel_schema_migrations.version`
/// (the migration timestamp). On a current database it starts a deferred
/// transaction, sees that every migration is present and `PRAGMA user_version`
/// already matches, and returns `Ok(0)` without committing. A pending migration
/// tries to write SQL, which a read-only connection rejects.
fn ensure_migrations_current(storage: &Path) -> Result<()> {
    let path = DbHandle::db_file_path(storage);
    if !path.is_file() {
        return Err(BridgeError::not_ready(OPEN_IN_GITBUTLER));
    }
    let mut connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| BridgeError::not_ready(OPEN_IN_GITBUTLER))?;
    match but_db::migration::run(&mut connection, but_db::migration::ours()) {
        Ok(0) => Ok(()),
        Ok(_) => Err(BridgeError::not_ready(OPEN_IN_GITBUTLER)),
        Err(_) => Err(BridgeError::not_ready(OPEN_IN_GITBUTLER)),
    }
}

fn storage_dir(worktree: &Path) -> Result<PathBuf> {
    let repo = gix::open_opts(worktree, gix::open::Options::isolated()).map_err(|error| {
        BridgeError::internal(format!("Could not open {}: {error}", worktree.display()))
    })?;
    repo.gitbutler_storage_path().map_err(|error| {
        BridgeError::internal(format!("Could not locate the project database: {error}"))
    })
}

fn worktree_path(project: &Project) -> Result<PathBuf> {
    let value = serde_json::to_value(project).map_err(|error| {
        BridgeError::internal(format!("Could not read the project path: {error}"))
    })?;
    let path = value
        .get("path")
        .and_then(|path| path.as_str())
        .filter(|path| !path.is_empty())
        .ok_or_else(|| BridgeError::internal("This project has no worktree path"))?;
    Ok(PathBuf::from(path))
}

/// DB heads are stored base-first (`position` ascending, tip last). Workspace
/// metadata wants the tip first, matching `VirtualBranchesTomlMetadata`.
fn workspace_from_snapshot(snapshot: &but_db::VirtualBranchesSnapshot) -> Result<Workspace> {
    let mut stacks = snapshot.stacks.clone();
    stacks.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut workspace_stacks = Vec::new();
    for stack in stacks {
        let mut heads: Vec<_> = snapshot
            .heads
            .iter()
            .filter(|head| head.stack_id == stack.id)
            .collect();
        heads.sort_by_key(|head| head.position);
        if heads.is_empty() {
            continue;
        }
        let branches = heads
            .into_iter()
            .rev()
            .filter_map(|head| {
                full_branch_name(&head.name).map(|ref_name| WorkspaceStackBranch {
                    ref_name,
                    archived: head.archived,
                })
            })
            .collect::<Vec<_>>();
        if branches.is_empty() {
            continue;
        }
        let id = StackId::from_str(&stack.id).map_err(|error| {
            BridgeError::internal(format!("Stack id '{}' is not a uuid: {error}", stack.id))
        })?;
        workspace_stacks.push(WorkspaceStack {
            id,
            branches,
            workspacecommit_relation: if stack.in_workspace {
                WorkspaceCommitRelation::Merged
            } else {
                WorkspaceCommitRelation::Outside
            },
        });
    }
    Ok(Workspace {
        // `created_at` marks the ref as managed so the graph treats it as the workspace.
        ref_info: MetaRefInfo {
            created_at: Some(gix::date::Time::now_local_or_utc()),
            updated_at: None,
        },
        stacks: workspace_stacks,
    })
}

fn full_branch_name(name: &str) -> Option<gix::refs::FullName> {
    if name.starts_with("refs/") {
        gix::refs::FullName::try_from(name).ok()
    } else {
        gix::refs::Category::LocalBranch.to_full_name(name).ok()
    }
}

/// `AppSettings::load` (`but-settings/src/persistence.rs`) creates `settings.json`
/// when the path is missing (`create_dirs_then_write("{}\n")`). It then calls
/// `maybe_migrate_legacy_settings` (`but-settings/src/legacy_settings.rs`), which
/// rewrites `settings.json` when `telemetry.migratedFromLegacy` is not true and
/// the legacy Tauri store `<app data>/settings.json` exists.
///
/// Call this before `AppSettings::load` or `AppSettingsWithDiskSync::new_with_customization`.
/// A missing settings file, or an unfinished legacy migration, returns an error
/// so neither path can create or rewrite the file. `Context::new_from_legacy_project_id`
/// loads settings the same way, so it is gated by this check too.
pub(crate) fn settings_safe_to_load(env: &Environment) -> Result<()> {
    let settings = env.config_dir.join(SETTINGS_FILE);
    if !settings.is_file() {
        return Err(BridgeError::not_ready(
            "Open GitButler once so it can create its settings. gb-gpui will not create that file.",
        ));
    }
    let raw = std::fs::read_to_string(&settings).map_err(|error| {
        BridgeError::not_ready(format!(
            "Open GitButler once so it can create its settings ({error})"
        ))
    })?;
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let migrated = value
        .get("telemetry")
        .and_then(|telemetry| telemetry.get("migratedFromLegacy"))
        .and_then(|flag| flag.as_bool())
        .unwrap_or(false);
    let legacy = env.app_data_dir.join(SETTINGS_FILE);
    if !migrated && legacy.is_file() {
        return Err(BridgeError::not_ready(
            "Open GitButler once so it can finish moving its settings. gb-gpui will not rewrite settings.json.",
        ));
    }
    Ok(())
}

/// Version and settings preflight for adding a repository.
///
/// `projects.json` may be missing: adding a project is what creates it.
/// Settings must already exist and be safe to load. This does not create them.
pub(crate) fn prepared_to_add(source: VersionSource) -> Result<Environment> {
    let env = environment(source)?;
    if env.compat != Compat::Match {
        return Err(compat_error(&env));
    }
    settings_safe_to_load(&env)?;
    Ok(env)
}

pub(crate) fn project_summary(project: Project) -> Result<ProjectSummary> {
    let id = project.id.to_string();
    let title = project.title.clone();
    let project = project.migrated().map_err(|error| {
        BridgeError::not_ready(format!("Open this project in GitButler first ({error})"))
    })?;
    let path = worktree_path(&project)?;
    Ok(ProjectSummary {
        id,
        title,
        path: path.display().to_string(),
    })
}

/// Version, channel, project files, and migrations. Does not open a `Context`.
pub(crate) fn prepared_project(source: VersionSource, id: &str) -> Result<Environment> {
    let env = environment(source)?;
    if env.compat != Compat::Match {
        return Err(compat_error(&env));
    }
    if !env.app_data_dir.join(PROJECTS_FILE).is_file()
        || !env.config_dir.join(SETTINGS_FILE).is_file()
    {
        return Err(BridgeError::not_ready(
            "Open GitButler once so it can create its project list and settings. gb-gpui will not create those files.",
        ));
    }
    settings_safe_to_load(&env)?;
    let projects =
        gitbutler_project::dangerously_list_projects_without_migration().map_err(|error| {
            BridgeError::internal(format!("Could not read the project list: {error}"))
        })?;
    let project = projects
        .into_iter()
        .find(|project| project.id.to_string() == id)
        .ok_or_else(|| BridgeError::not_found(format!("No project with id {id}")))?;
    let project = project.migrated().map_err(|error| {
        BridgeError::not_ready(format!("Open this project in GitButler first ({error})"))
    })?;
    let worktree = worktree_path(&project)?;
    let storage = storage_dir(&worktree)?;
    ensure_migrations_current(&storage)?;
    Ok(env)
}

/// Worktree of a project that already passes the read preflight.
pub(crate) fn project_worktree(source: VersionSource, id: &str) -> Result<PathBuf> {
    prepared_project(source, id)?;
    let projects =
        gitbutler_project::dangerously_list_projects_without_migration().map_err(|error| {
            BridgeError::internal(format!("Could not read the project list: {error}"))
        })?;
    let project = projects
        .into_iter()
        .find(|project| project.id.to_string() == id)
        .ok_or_else(|| BridgeError::not_found(format!("No project with id {id}")))?;
    let project = project.migrated().map_err(|error| {
        BridgeError::not_ready(format!("Open this project in GitButler first ({error})"))
    })?;
    worktree_path(&project)
}

pub(crate) fn summarize(id: &str, info: &but_workspace::RefInfo) -> WorkspaceSummary {
    let stacks = info
        .stacks
        .iter()
        .map(|stack| {
            let branches = stack
                .segments
                .iter()
                .filter_map(branch_from_segment)
                .collect();
            StackSummary { branches }
        })
        .collect();
    WorkspaceSummary {
        project_id: id.to_string(),
        target_branch: info
            .target_ref
            .as_ref()
            .map(|target| target.ref_name.to_string()),
        stacks,
    }
}

/// Local commits only. `commits_on_remote` are upstream-only and omitted.
fn branch_from_segment(segment: &but_workspace::ref_info::Segment) -> Option<BranchSummary> {
    let name = segment
        .ref_info
        .as_ref()
        .map(|ref_info| branch_label(ref_info.ref_name.as_ref()))
        .filter(|name| !name.is_empty())?;
    let commits = segment
        .commits
        .iter()
        .map(|commit| {
            let sha = commit.id.to_string();
            CommitSummary {
                short_sha: short_sha(commit.id),
                title: first_line(&commit.message),
                author: person_label(&commit.author),
                authored_at: commit.author.time.seconds,
                conflicted: commit.has_conflicts,
                pushed: matches!(
                    commit.relation,
                    but_workspace::ref_info::LocalCommitRelation::LocalAndRemote(_)
                ),
                sha,
            }
        })
        .collect::<Vec<_>>();
    let tip_sha = commits
        .first()
        .map(|commit| commit.short_sha.clone())
        .unwrap_or_else(|| segment.tip().map(short_sha).unwrap_or_default());
    let tip_full = commits
        .first()
        .map(|commit| commit.sha.clone())
        .unwrap_or_default();
    Some(BranchSummary {
        name,
        tip_sha,
        tip_full,
        commits,
    })
}

pub(crate) fn person_label(signature: &gix::actor::Signature) -> String {
    let name = lossy_bytes(&signature.name);
    let email = lossy_bytes(&signature.email);
    if email.is_empty() {
        name
    } else {
        format!("{name} <{email}>")
    }
}

fn installed_short_version() -> Option<String> {
    let entries = std::fs::read_dir("/Applications").ok()?;
    let mut apps: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("app")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with("GitButler"))
        })
        .collect();
    apps.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name != "GitButler.app")
            .unwrap_or(true)
    });
    apps.into_iter()
        .find_map(|app| plist_short_version(&app.join("Contents/Info.plist")))
}

fn plist_short_version(plist: &Path) -> Option<String> {
    let output = std::process::Command::new("plutil")
        .args([
            "-extract",
            "CFBundleShortVersionString",
            "raw",
            &plist.display().to_string(),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8(output.stdout).ok()?;
    let version = version.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::channel_error;
    use crate::{BridgeErrorKind, RELEASE_IDENTIFIER, WRONG_CHANNEL_MESSAGE};

    #[test]
    fn a_dev_channel_identifier_is_rejected() {
        let error = channel_error("com.gitbutler.app.dev").expect_err("dev channel");
        assert_eq!(error.kind, BridgeErrorKind::Compat);
        assert_eq!(error.message, WRONG_CHANNEL_MESSAGE);
        assert!(channel_error("com.gitbutler.app.nightly").is_err());
    }

    #[test]
    fn the_release_identifier_is_accepted() {
        channel_error(RELEASE_IDENTIFIER).expect("release");
        // This crate's tests run from apps/gb-gpui, where CHANNEL=release.
        assert_eq!(but_path::identifier(), RELEASE_IDENTIFIER);
        channel_error(but_path::identifier()).expect("this build");
    }
}
