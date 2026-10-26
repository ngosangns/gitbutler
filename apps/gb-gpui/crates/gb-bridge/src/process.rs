//! Real backend. Blocking GitButler calls run on the caller's thread.
//! `gitbutler_watcher::watch_in_background` uses `tokio::spawn`, so `watch`
//! enters this runtime. Call it from outside that runtime.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use but_settings::AppSettingsWithDiskSync;
use tokio::sync::mpsc;

use crate::askpass;
use crate::branches;
use crate::edit;
use crate::forge::Forge;
use crate::history;
use crate::live;
use crate::messages::MessageGenerator;
use crate::mutate;
use crate::read::{self, VersionSource};
use crate::remote;
use crate::settings::{self, GlobalPatch, GlobalSettings, ProjectSettings, Settings};
use crate::{
    Anchor, ApplyResult, AskpassRequest, AskpassWatch, Backend, BaseBranchData, BranchDetails,
    BranchRow, BridgeError, BridgeEvent, CommitDetails, CommitResult, ConflictedCommit,
    DiffRequest, DiscardResult, EditStatus, Environment, FetchReport, FetchStatus, FileChoice,
    ForcePush, ForgeStatus, HookReport, IntegratePreview, IntegrateReport, LaneCreated,
    ProjectList, ProjectSummary, PushReport, RemoteBranch, ResolveReport, Result, Review,
    SelectionSpec, SnapshotChange, SnapshotInfo, SuggestedMessage, TextDiff, UndoResult,
    UpstreamStrategy, WatchHandle, WorkspaceSummary, WorktreeView,
};

pub struct InProcessBackend {
    runtime: tokio::runtime::Runtime,
    source: VersionSource,
    hub: Arc<Hub>,
}

impl InProcessBackend {
    pub fn new() -> Result<Self> {
        Self::build(VersionSource::Detect)
    }

    /// Tests pass `Some("9.9.9")` to force a mismatch and `Some("0.22.3")` to
    /// force a match. `None` forces "not installed". Production uses [`Self::new`].
    ///
    /// The askpass stub is installed first so a later fetch against a local
    /// file remote can stat a helper. It does not replace `GITBUTLER_ASKPASS_BIN`
    /// when that variable is already set.
    pub fn with_installed_version_for_test(version: Option<&'static str>) -> Result<Self> {
        askpass::install_fetch_stub()?;
        Self::build(VersionSource::Override(version))
    }

    /// How long a credential prompt waits before it is cancelled. Tests use a
    /// short wait. Production stays at two minutes.
    pub fn set_askpass_timeout_for_test(&self, timeout: std::time::Duration) {
        askpass::set_timeout(timeout);
    }

    fn build(source: VersionSource) -> Result<Self> {
        askpass::install()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("gb-bridge")
            .enable_all()
            .build()
            .map_err(|error| {
                BridgeError::internal(format!("Could not start the file watcher runtime: {error}"))
            })?;
        Ok(Self {
            runtime,
            source,
            hub: Arc::new(Hub::default()),
        })
    }

    /// One mutation per project. The permit stays held while a successful call
    /// emits refresh events, so a second call on the same project waits until
    /// those events are queued.
    fn mutate<T>(&self, id: &str, refresh: bool, op: impl FnOnce() -> Result<T>) -> Result<T> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.hub.call(
            &project,
            || {
                if refresh {
                    refresh_now(source, &hub, &project);
                }
            },
            op,
        )
    }
}

fn refresh_now(source: VersionSource, hub: &Hub, id: &str) {
    if let Ok(view) = live::worktree(source, id) {
        hub.emit(id, BridgeEvent::Worktree(view));
    }
    hub.emit(id, BridgeEvent::WorkspaceActivity);
}

/// Per-project queue, exclusive-owner count, and mutation listeners.
pub(crate) struct Hub {
    inner: Mutex<HashMap<String, ProjectGate>>,
    tokens: AtomicU64,
    /// Serializes add and delete, which both rewrite `projects.json`.
    /// Delete holds the project queue first, then this lock. Add holds only
    /// this lock, so the two cannot deadlock.
    projects: Mutex<()>,
}

struct ProjectGate {
    queue: Arc<Mutex<()>>,
    exclusive_owners: usize,
    listeners: Vec<(u64, mpsc::UnboundedSender<BridgeEvent>)>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            tokens: AtomicU64::new(1),
            projects: Mutex::new(()),
        }
    }
}

impl Hub {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ProjectGate>> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn gate<'a>(inner: &'a mut HashMap<String, ProjectGate>, id: &str) -> &'a mut ProjectGate {
        inner.entry(id.to_string()).or_insert_with(|| ProjectGate {
            queue: Arc::new(Mutex::new(())),
            exclusive_owners: 0,
            listeners: Vec::new(),
        })
    }

    pub(crate) fn queue(&self, id: &str) -> Arc<Mutex<()>> {
        Arc::clone(&Self::gate(&mut self.lock(), id).queue)
    }

    /// `true` when a watch in this process holds `project.lock`.
    pub(crate) fn owns(&self, id: &str) -> bool {
        self.lock()
            .get(id)
            .is_some_and(|gate| gate.exclusive_owners > 0)
    }

    pub(crate) fn acquire(&self, id: &str) {
        Self::gate(&mut self.lock(), id).exclusive_owners += 1;
    }

    pub(crate) fn release(&self, id: &str) {
        let mut inner = self.lock();
        let Some(gate) = inner.get_mut(id) else {
            return;
        };
        gate.exclusive_owners = gate.exclusive_owners.saturating_sub(1);
    }

    pub(crate) fn listen(&self, id: &str, tx: mpsc::UnboundedSender<BridgeEvent>) -> u64 {
        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        Self::gate(&mut self.lock(), id).listeners.push((token, tx));
        token
    }

    pub(crate) fn unlisten(&self, id: &str, token: u64) {
        let mut inner = self.lock();
        let Some(gate) = inner.get_mut(id) else {
            return;
        };
        gate.listeners.retain(|(have, _)| *have != token);
    }

    pub(crate) fn projects_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.projects
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(crate) fn emit(&self, id: &str, event: BridgeEvent) {
        let senders: Vec<_> = self
            .lock()
            .get(id)
            .map(|gate| gate.listeners.iter().map(|(_, tx)| tx.clone()).collect())
            .unwrap_or_default();
        for tx in senders {
            let _ = tx.send(event.clone());
        }
    }

    fn call<T>(
        &self,
        id: &str,
        on_success: impl FnOnce(),
        op: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let queue = self.queue(id);
        let _permit = queue.lock().unwrap_or_else(|poison| poison.into_inner());
        match std::panic::catch_unwind(AssertUnwindSafe(op)) {
            Ok(Ok(value)) => {
                on_success();
                Ok(value)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(BridgeError::internal(
                "GitButler panicked while changing this project.",
            )),
        }
    }
}

impl Backend for InProcessBackend {
    fn environment(&self) -> Result<Environment> {
        read::environment(self.source)
    }

    fn list_projects(&self) -> Result<ProjectList> {
        read::list_projects(self.source)
    }

    fn open_project(&self, id: &str) -> Result<WorkspaceSummary> {
        read::open_project(self.source, id)
    }

    fn worktree(&self, id: &str) -> Result<WorktreeView> {
        live::worktree(self.source, id)
    }

    fn commit_details(&self, id: &str, sha: &str) -> Result<CommitDetails> {
        live::commit_details(self.source, id, sha)
    }

    fn file_diff(&self, id: &str, request: DiffRequest) -> Result<TextDiff> {
        live::file_diff(self.source, id, request)
    }

    fn watch(
        &self,
        id: &str,
        on_change: Box<dyn Fn(BridgeEvent) + Send + Sync>,
    ) -> Result<WatchHandle> {
        // Order matches the installed app (`gitbutler-tauri` `window.rs`,
        // `but-server` `projects.rs`), with the write gates first.
        //
        // 1. `open_context` runs `prepared_project`: version pin, release
        //    channel, `projects.json` and `settings.json` already exist, no
        //    pending migration. `settings_safe_to_load` refuses a missing
        //    settings file and an unfinished legacy Tauri-store migration.
        //    `AppSettings::load` (`but-settings/src/persistence.rs`) would
        //    otherwise create `settings.json`, and `maybe_migrate_legacy_settings`
        //    (`but-settings/src/legacy_settings.rs`) would rewrite it.
        // 2. Only then does `Context::try_from` load settings, and only then
        //    does `AppSettingsWithDiskSync::new_with_customization` load them
        //    again for the watcher. Both are the same load the app uses.
        // 3. `try_exclusive_access` takes `.git/project.lock` when this is the
        //    only GitButler process. A failure is shown in the window; the
        //    watcher still starts. This crate never creates a second writer
        //    to hide that.
        let mut ctx = live::open_context(self.source, id)?;
        let env = read::environment(self.source)?;
        let app_settings = AppSettingsWithDiskSync::new_with_customization(&env.config_dir, None)
            .map_err(|error| {
            BridgeError::internal(format!("Could not read GitButler settings: {error}"))
        })?;
        let watch_mode = {
            let settings = app_settings.get().map_err(|error| {
                BridgeError::internal(format!("Could not read GitButler settings: {error}"))
            })?;
            gitbutler_watcher::WatchMode::from_env_or_settings(
                &settings.feature_flags.watch_mode,
                |key| std::env::var(key).ok(),
            )
        };
        // A second `project.lock` in this process can drop the watcher's lock
        // when it is closed (`flock` is per process). Skip it when we already
        // hold one.
        let already = self.hub.owns(id);
        let lock = if already {
            None
        } else {
            ctx.try_exclusive_access().ok()
        };
        let exclusive = lock.is_some() || already;
        let worktree = ctx.workdir_or_fail().map_err(|error| {
            BridgeError::internal(format!("Could not find the worktree: {error}"))
        })?;
        let project_id = ctx.legacy_project.id.clone();
        drop(ctx);

        let (tx, rx) = mpsc::unbounded_channel();
        let token = self.hub.listen(id, tx.clone());
        let held = lock.is_some();
        if held {
            self.hub.acquire(id);
        }
        let handler = gitbutler_watcher::Handler::new(move |change| {
            let _ = tx.send(map_change(change));
            Ok(())
        });
        let watched = worktree.display().to_string();
        let watcher = self
            .runtime
            .block_on(async move {
                gitbutler_watcher::watch_in_background(
                    handler,
                    &worktree,
                    project_id,
                    app_settings,
                    watch_mode,
                )
            })
            .map_err(|error| {
                self.hub.unlisten(id, token);
                if held {
                    self.hub.release(id);
                }
                BridgeError::internal(format!("Could not watch {watched}: {error}"))
            })?;
        let on_change: Arc<dyn Fn(BridgeEvent) + Send + Sync> = Arc::from(on_change);
        let task = self.runtime.spawn(debounce(rx, on_change));
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        let cleanup = Box::new(move || {
            hub.unlisten(&project, token);
            if held {
                hub.release(&project);
            }
        });
        Ok(WatchHandle::live(
            watcher,
            lock,
            task,
            exclusive,
            Some(cleanup),
        ))
    }

    fn assign(
        &self,
        id: &str,
        selections: &[SelectionSpec],
        lane: Option<&str>,
    ) -> Result<WorkspaceSummary> {
        let selections = selections.to_vec();
        let lane = lane.map(str::to_string);
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::assign(source, &project, &hub, &selections, lane.as_deref())
        })
    }

    fn discard(&self, id: &str, selections: &[SelectionSpec]) -> Result<DiscardResult> {
        let selections = selections.to_vec();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::discard(source, &project, &hub, &selections)
        })
    }

    fn create_lane(&self, id: &str, name: &str) -> Result<LaneCreated> {
        let name = name.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::create_lane(source, &project, &hub, &name)
        })
    }

    fn commit_create(
        &self,
        id: &str,
        anchor: &Anchor,
        selections: &[SelectionSpec],
        message: &str,
    ) -> Result<CommitResult> {
        let anchor = anchor.clone();
        let selections = selections.to_vec();
        let message = message.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::commit_create(source, &project, &hub, &anchor, &selections, &message)
        })
    }

    fn amend(&self, id: &str, sha: &str, selections: &[SelectionSpec]) -> Result<CommitResult> {
        let sha = sha.to_string();
        let selections = selections.to_vec();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::amend(source, &project, &hub, &sha, &selections)
        })
    }

    fn reword(&self, id: &str, sha: &str, message: &str) -> Result<CommitResult> {
        let sha = sha.to_string();
        let message = message.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::reword(source, &project, &hub, &sha, &message)
        })
    }

    fn squash(&self, id: &str, subject_shas: &[String], target_sha: &str) -> Result<CommitResult> {
        let subject_shas = subject_shas.to_vec();
        let target_sha = target_sha.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::squash(source, &project, &hub, &subject_shas, &target_sha)
        })
    }

    fn uncommit(&self, id: &str, shas: &[String], lane: Option<&str>) -> Result<CommitResult> {
        let shas = shas.to_vec();
        let lane = lane.map(str::to_string);
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::uncommit(source, &project, &hub, &shas, lane.as_deref())
        })
    }

    fn uncommit_changes(
        &self,
        id: &str,
        sha: &str,
        selections: &[SelectionSpec],
        lane: Option<&str>,
    ) -> Result<CommitResult> {
        let sha = sha.to_string();
        let selections = selections.to_vec();
        let lane = lane.map(str::to_string);
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::uncommit_changes(source, &project, &hub, &sha, &selections, lane.as_deref())
        })
    }

    fn move_commit(&self, id: &str, shas: &[String], anchor: &Anchor) -> Result<CommitResult> {
        let shas = shas.to_vec();
        let anchor = anchor.clone();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::move_commit(source, &project, &hub, &shas, &anchor)
        })
    }

    fn insert_blank(&self, id: &str, anchor: &Anchor) -> Result<CommitResult> {
        let anchor = anchor.clone();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::insert_blank(source, &project, &hub, &anchor)
        })
    }

    fn move_changes(
        &self,
        id: &str,
        source_sha: &str,
        destination_sha: &str,
        selections: &[SelectionSpec],
    ) -> Result<CommitResult> {
        let source_sha = source_sha.to_string();
        let destination_sha = destination_sha.to_string();
        let selections = selections.to_vec();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::move_changes(
                source,
                &project,
                &hub,
                &source_sha,
                &destination_sha,
                &selections,
            )
        })
    }

    fn snapshots(&self, id: &str, limit: usize) -> Result<Vec<SnapshotInfo>> {
        let source = self.source;
        let project = id.to_string();
        self.mutate(id, false, move || {
            mutate::snapshots(source, &project, limit)
        })
    }

    fn restore_snapshot(&self, id: &str, sha: &str) -> Result<UndoResult> {
        let sha = sha.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            mutate::restore_snapshot(source, &project, &hub, &sha)
        })
    }

    fn undo_last(&self, id: &str) -> Result<UndoResult> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || mutate::undo_last(source, &project, &hub))
    }

    fn snapshot_diff(&self, id: &str, sha: &str) -> Result<Vec<SnapshotChange>> {
        let sha = sha.to_string();
        let source = self.source;
        let project = id.to_string();
        self.mutate(id, false, move || {
            history::snapshot_diff(source, &project, &sha)
        })
    }

    fn create_snapshot(&self, id: &str, message: &str) -> Result<SnapshotInfo> {
        let message = message.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            history::create_snapshot(source, &project, &hub, &message)
        })
    }

    fn list_branches(&self, id: &str) -> Result<Vec<BranchRow>> {
        let source = self.source;
        let project = id.to_string();
        self.mutate(id, false, move || branches::list_branches(source, &project))
    }

    fn branch_details(&self, id: &str, ref_name: &str) -> Result<BranchDetails> {
        let ref_name = ref_name.to_string();
        let source = self.source;
        let project = id.to_string();
        self.mutate(id, false, move || {
            branches::branch_details(source, &project, &ref_name)
        })
    }

    fn apply_branch(&self, id: &str, ref_name: &str) -> Result<ApplyResult> {
        let ref_name = ref_name.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            branches::apply_branch(source, &project, &hub, &ref_name)
        })
    }

    fn unapply_branch(&self, id: &str, name: &str) -> Result<WorkspaceSummary> {
        let name = name.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            branches::unapply_branch(source, &project, &hub, &name)
        })
    }

    fn delete_local_branch(&self, id: &str, name: &str) -> Result<WorkspaceSummary> {
        let name = name.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            branches::delete_local_branch(source, &project, &hub, &name)
        })
    }

    fn edit_status(&self, id: &str) -> Result<EditStatus> {
        let source = self.source;
        let project = id.to_string();
        self.mutate(id, false, move || edit::edit_status(source, &project))
    }

    fn enter_edit_mode(&self, id: &str, sha: &str) -> Result<EditStatus> {
        let sha = sha.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            edit::enter_edit_mode(source, &project, &hub, &sha)
        })
    }

    fn save_edit_and_return_to_workspace(&self, id: &str) -> Result<WorkspaceSummary> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || edit::save_edit(source, &project, &hub))
    }

    fn abort_edit_and_return_to_workspace(
        &self,
        id: &str,
        force: bool,
    ) -> Result<WorkspaceSummary> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            edit::abort_edit(source, &project, &hub, force)
        })
    }

    fn conflicted_commits(&self, id: &str) -> Result<Vec<ConflictedCommit>> {
        let source = self.source;
        let project = id.to_string();
        self.mutate(id, false, move || {
            edit::conflicted_commits(source, &project)
        })
    }

    fn resolve_commit_conflict_hunks(
        &self,
        id: &str,
        sha: &str,
        files: &[FileChoice],
    ) -> Result<ResolveReport> {
        let sha = sha.to_string();
        let files = files.to_vec();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            edit::resolve_conflicts(source, &project, &hub, &sha, &files)
        })
    }

    fn pre_commit_hook(&self, id: &str, selections: &[SelectionSpec]) -> Result<HookReport> {
        let selections = selections.to_vec();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, false, move || {
            mutate::pre_commit_hook(source, &project, &hub, &selections)
        })
    }

    fn post_commit_hook(&self, id: &str) -> Result<HookReport> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, false, move || {
            mutate::post_commit_hook(source, &project, &hub)
        })
    }

    fn message_hook(&self, id: &str, message: &str) -> Result<HookReport> {
        let message = message.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, false, move || {
            mutate::message_hook(source, &project, &hub, &message)
        })
    }

    fn add_project(&self, path: &str) -> Result<ProjectSummary> {
        let source = self.source;
        let path = path.to_string();
        let hub = Arc::clone(&self.hub);
        self.hub.call(
            remote::projects_queue_key(),
            || {},
            move || {
                let _projects = hub.projects_lock();
                remote::add_project(source, &path)
            },
        )
    }

    fn delete_project(&self, id: &str) -> Result<()> {
        let source = self.source;
        let project = id.to_string();
        let hub = Arc::clone(&self.hub);
        let queued = project.clone();
        self.hub.call(
            &queued,
            || {},
            move || {
                let _projects = hub.projects_lock();
                remote::delete_project(source, &project)
            },
        )
    }

    fn base_branch(&self, id: &str) -> Result<Option<BaseBranchData>> {
        remote::base_branch(self.source, id)
    }

    fn remote_branches(&self, id: &str) -> Result<Vec<RemoteBranch>> {
        remote::remote_branches(self.source, id)
    }

    fn set_target(
        &self,
        id: &str,
        target_ref: &str,
        push_remote: Option<&str>,
    ) -> Result<BaseBranchData> {
        let target_ref = target_ref.to_string();
        let push_remote = push_remote.map(str::to_string);
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            remote::set_target(source, &project, &hub, &target_ref, push_remote.as_deref())
        })
    }

    fn fetch(&self, id: &str) -> Result<FetchReport> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            let report = remote::fetch(source, &project, &hub)?;
            hub.emit(&project, BridgeEvent::GitFetch);
            Ok(report)
        })
    }

    fn fetch_status(&self, id: &str) -> Result<FetchStatus> {
        remote::fetch_status(self.source, id)
    }

    fn integrate_preview(&self, id: &str, strategy: UpstreamStrategy) -> Result<IntegratePreview> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, false, move || {
            remote::preview(source, &project, &hub, strategy)
        })
    }

    fn integrate(&self, id: &str, strategy: UpstreamStrategy) -> Result<IntegrateReport> {
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            remote::apply(source, &project, &hub, strategy)
        })
    }

    fn push(&self, id: &str, lane: &str, force: ForcePush) -> Result<PushReport> {
        let lane = lane.to_string();
        let source = self.source;
        let hub = Arc::clone(&self.hub);
        let project = id.to_string();
        self.mutate(id, true, move || {
            let report = remote::push(source, &project, &hub, &lane, force)?;
            hub.emit(&project, BridgeEvent::GitFetch);
            Ok(report)
        })
    }

    fn askpass_pending(&self) -> Result<Vec<AskpassRequest>> {
        askpass::pending()
    }

    fn watch_askpass(
        &self,
        on_prompt: Box<dyn Fn(AskpassRequest) + Send + Sync>,
    ) -> Result<AskpassWatch> {
        askpass::watch(on_prompt)
    }

    fn answer_askpass(&self, id: &str, response: Option<String>) -> Result<()> {
        askpass::answer(id, response)
    }

    fn forge_status(&self, id: &str) -> Result<ForgeStatus> {
        let _ = (self, id);
        crate::forge::DisconnectedForge.reviews(id)
    }

    fn review_for_branch(&self, id: &str, branch: &str) -> Result<Option<Review>> {
        let _ = self;
        crate::forge::DisconnectedForge.review_for_branch(id, branch)
    }

    fn publish_draft(&self, id: &str, branch: &str) -> Result<Review> {
        let _ = self;
        crate::forge::DisconnectedForge.publish_draft(id, branch)
    }

    fn suggest_message(&self, id: &str, paths: &[String]) -> Result<SuggestedMessage> {
        let _ = (self, id);
        crate::messages::UnavailableMessageGenerator.suggest(paths)
    }
}

impl Settings for InProcessBackend {
    fn read_global(&self) -> Result<GlobalSettings> {
        settings::disk::read_global(self.source)
    }

    fn update_global(&self, patch: &GlobalPatch) -> Result<GlobalSettings> {
        settings::disk::update_global(self.source, patch)
    }

    fn read_project(&self, id: &str) -> Result<ProjectSettings> {
        settings::disk::read_project(self.source, id)
    }

    fn forget_project(&self, id: &str) -> Result<()> {
        settings::disk::forget_guard()?;
        Backend::delete_project(self, id)
    }
}

fn map_change(change: gitbutler_watcher::Change) -> BridgeEvent {
    match change {
        gitbutler_watcher::Change::GitFetch(_) => BridgeEvent::GitFetch,
        gitbutler_watcher::Change::GitHead {
            head,
            operating_mode,
            ..
        } => BridgeEvent::GitHead {
            head,
            mode: mode_label(&operating_mode),
        },
        gitbutler_watcher::Change::GitActivity { head_sha, .. } => {
            BridgeEvent::GitActivity { head_sha }
        }
        gitbutler_watcher::Change::WorkspaceActivity { .. } => BridgeEvent::WorkspaceActivity,
        gitbutler_watcher::Change::WorktreeChanges {
            changes,
            changed_paths,
            ..
        } => {
            let paths = changed_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            BridgeEvent::Worktree(live::view_from_changes(&changes, paths))
        }
    }
}

/// `OperatingMode` is not a direct dependency. Its `Debug` text starts with the
/// variant name (`gitbutler-operating-modes`).
fn mode_label(mode: &impl std::fmt::Debug) -> String {
    let rendered = format!("{mode:?}");
    if rendered.starts_with("OpenWorkspace") {
        "workspace".to_string()
    } else if rendered.starts_with("Edit") {
        "edit".to_string()
    } else {
        "outside".to_string()
    }
}

struct Pending {
    worktree: Option<WorktreeView>,
    git: Vec<BridgeEvent>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.worktree.is_none() && self.git.is_empty()
    }
}

/// Keep the newest worktree snapshot and the union of its changed paths.
/// Keep the newest event of each other kind.
fn absorb(pending: &mut Pending, event: BridgeEvent) {
    match event {
        BridgeEvent::Worktree(mut view) => {
            if let Some(prior) = pending.worktree.take() {
                let mut paths = prior.changed_paths;
                for path in view.changed_paths.drain(..) {
                    if !paths.iter().any(|have| have == &path) {
                        paths.push(path);
                    }
                }
                view.changed_paths = paths;
            }
            pending.worktree = Some(view);
        }
        other => {
            let name = other.name();
            pending.git.retain(|have| have.name() != name);
            pending.git.push(other);
        }
    }
}

fn flush(pending: &mut Pending, on_change: &Arc<dyn Fn(BridgeEvent) + Send + Sync>) {
    for event in pending.git.drain(..) {
        on_change(event);
    }
    if let Some(view) = pending.worktree.take() {
        on_change(BridgeEvent::Worktree(view));
    }
}

/// Coalesce a burst so the window applies one quiet-period update, while still
/// delivering each kind of event that arrived.
async fn debounce(
    mut rx: mpsc::UnboundedReceiver<BridgeEvent>,
    on_change: Arc<dyn Fn(BridgeEvent) + Send + Sync>,
) {
    let mut pending = Pending {
        worktree: None,
        git: Vec::new(),
    };
    loop {
        tokio::select! {
            incoming = rx.recv() => {
                match incoming {
                    Some(event) => absorb(&mut pending, event),
                    None => break,
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)), if !pending.is_empty() => {
                flush(&mut pending, &on_change);
            }
        }
    }
    if !pending.is_empty() {
        flush(&mut pending, &on_change);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileChange, FileStatus};

    fn file(path: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 1,
            deletions: 0,
            hunks: Vec::new(),
        }
    }

    fn worktree(path: &str, changed: &str) -> BridgeEvent {
        BridgeEvent::Worktree(WorktreeView {
            files: vec![file(path)],
            changed_paths: vec![changed.to_string()],
        })
    }

    #[test]
    fn coalesce_unions_paths_and_keeps_the_latest_snapshot() {
        let mut pending = Pending {
            worktree: None,
            git: Vec::new(),
        };
        absorb(&mut pending, worktree("a.txt", "a.txt"));
        absorb(&mut pending, worktree("b.txt", "b.txt"));
        absorb(
            &mut pending,
            BridgeEvent::GitActivity {
                head_sha: "abc".to_string(),
            },
        );
        absorb(
            &mut pending,
            BridgeEvent::GitActivity {
                head_sha: "def".to_string(),
            },
        );
        absorb(&mut pending, BridgeEvent::WorkspaceActivity);
        let view = pending.worktree.expect("snapshot");
        assert_eq!(view.files[0].path, "b.txt");
        assert_eq!(view.changed_paths, ["a.txt", "b.txt"]);
        assert_eq!(pending.git.len(), 2);
        assert_eq!(
            pending.git[0],
            BridgeEvent::GitActivity {
                head_sha: "def".to_string(),
            }
        );
        assert_eq!(pending.git[1], BridgeEvent::WorkspaceActivity);
    }

    #[test]
    fn one_project_runs_one_mutation_at_a_time() {
        use std::sync::atomic::AtomicUsize;
        use std::thread;

        let hub = Arc::new(Hub::default());
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..4 {
            let hub = Arc::clone(&hub);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            threads.push(thread::spawn(move || {
                hub.call(
                    "project",
                    || {},
                    || {
                        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(40));
                        current.fetch_sub(1, Ordering::SeqCst);
                        Ok::<(), BridgeError>(())
                    },
                )
                .unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panic_in_a_mutation_becomes_an_error() {
        let hub = Hub::default();
        let error = hub
            .call("project", || {}, || -> Result<()> { panic!("boom") })
            .expect_err("panic");
        assert_eq!(error.kind, crate::BridgeErrorKind::Internal);
        assert_eq!(
            error.message,
            "GitButler panicked while changing this project."
        );
    }
}
