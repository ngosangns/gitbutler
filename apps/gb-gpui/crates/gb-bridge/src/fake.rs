//! Canned backend for screenshots and store tests. It never touches GitButler files.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::forge::{self, Forge, ForgeStatus, Review};
use crate::messages::{FakeMessageGenerator, MessageGenerator, SuggestedMessage};
use crate::settings::{GlobalPatch, GlobalSettings, ProjectSettings, Settings, SigningReport};
use crate::{
    Anchor, ApplyResult, AskpassKind, AskpassRequest, AskpassWatch, AssignedHunk, Backend,
    BaseBranchData, BranchDetails, BranchPlace, BranchRow, BranchSummary, BridgeError, BridgeEvent,
    CommitDetails, CommitFile, CommitResult, CommitSummary, Compat, ConflictChoice,
    ConflictedCommit, DiffRequest, DiscardResult, EDIT_MODE_MESSAGE, EditStatus, Environment,
    FakeWatch, FetchReport, FetchStatus, FileChange, FileChoice, FileStatus, ForcePush, HookReport,
    HookStatus, HunkRef, IntegratePreview, IntegrateReport, IntegrateResolution, LaneCreated,
    LaneResolution, PINNED_VERSION, ProjectList, ProjectSummary, PushReport, PushedBranch,
    RemoteBranch, ResolveReport, Result, SelectionSpec, SnapshotChange, SnapshotInfo, StackSummary,
    TextDiff, UndoResult, UpstreamCommit, UpstreamStrategy, WatchHandle, WorkspaceSummary,
    WorktreeView,
};

#[derive(Clone)]
struct FakeUpstream {
    sha: String,
    short_sha: String,
    title: String,
    author: String,
}

struct FakeRemote {
    waiting: Vec<FakeUpstream>,
    fetched: Vec<FakeUpstream>,
    last_fetched_ms: Option<u128>,
    last_attempted_ms: Option<u128>,
    last_error: Option<String>,
    push_remote: String,
}

impl FakeRemote {
    fn demo() -> Self {
        Self {
            waiting: vec![
                fake_upstream("d11d11d", "Upstream one"),
                fake_upstream("d22d22d", "Upstream two"),
            ],
            fetched: Vec::new(),
            last_fetched_ms: None,
            last_attempted_ms: None,
            last_error: None,
            push_remote: "origin".to_string(),
        }
    }
}

struct FakeState {
    env: Environment,
    projects: Vec<ProjectSummary>,
    workspaces: HashMap<String, WorkspaceSummary>,
    worktrees: HashMap<String, WorktreeView>,
    commits: HashMap<String, CommitDetails>,
    diffs: HashMap<String, TextDiff>,
    list_reason: Option<String>,
    next_sha: u64,
    history: HashMap<String, Vec<FakeImage>>,
    remotes: HashMap<String, FakeRemote>,
    askpass: Vec<AskpassRequest>,
    /// Branches that are not applied. Applied lanes stay on the workspace.
    catalog: HashMap<String, Vec<CatalogBranch>>,
    edits: HashMap<String, FakeEdit>,
    /// Pull requests kept in memory. The demo project starts with a canned set.
    reviews: HashMap<String, Vec<Review>>,
    next_review: u64,
    global: GlobalSettings,
}

#[derive(Clone)]
struct CatalogBranch {
    name: String,
    ref_name: String,
    place: BranchPlace,
    remotes: Vec<String>,
    tip_sha: String,
    tip_short: String,
    last_commit: String,
    author: String,
    updated_at_ms: Option<i64>,
    ahead: Option<usize>,
    behind: Option<usize>,
    commits: Vec<CommitSummary>,
}

#[derive(Clone)]
struct FakeEdit {
    sha: String,
    stack_id: String,
    lane: String,
    files: HashMap<String, String>,
}

#[derive(Clone)]
struct FakeImage {
    info: SnapshotInfo,
    workspace: WorkspaceSummary,
    worktree: WorktreeView,
    commits: HashMap<String, CommitDetails>,
    diffs: HashMap<String, TextDiff>,
    catalog: Vec<CatalogBranch>,
    edit: Option<FakeEdit>,
}

pub struct FakeBackend {
    state: Mutex<FakeState>,
    watchers: Arc<Mutex<HashMap<String, Vec<FakeWatch>>>>,
    askpass_watchers: Arc<Mutex<Vec<(u64, crate::askpass::PromptListener)>>>,
    tokens: AtomicU64,
    /// Extra wait before each gated call. Zero keeps tests instant.
    latency_ms: AtomicU64,
    /// Operations that should fail once. The name is removed before the wait.
    fail_ops: Mutex<HashSet<String>>,
}

impl FakeBackend {
    /// Three lanes, several commits, and a worktree a screenshot can open.
    pub fn demo() -> Self {
        let project = ProjectSummary {
            id: "demo-project".to_string(),
            title: "fixture".to_string(),
            path: "/tmp/fixture".to_string(),
        };
        let (workspace, worktree, commits, diffs) = demo_project(&project.id);
        Self::from_parts(
            environment_for(Compat::Match, Some(PINNED_VERSION.to_string())),
            vec![project],
            Some(workspace),
            Some(worktree),
            commits,
            diffs,
            None,
        )
    }

    pub fn mismatched(installed: &str) -> Self {
        Self::from_parts(
            environment_for(
                Compat::Mismatch {
                    installed: installed.to_string(),
                    pinned: PINNED_VERSION.to_string(),
                },
                Some(installed.to_string()),
            ),
            Vec::new(),
            None,
            None,
            HashMap::new(),
            HashMap::new(),
            None,
        )
    }

    pub fn trigger(&self, id: &str, event: BridgeEvent) {
        let callbacks: Vec<_> = self
            .watchers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(id)
            .map(|list| {
                list.iter()
                    .map(|watch| Arc::clone(&watch.callback))
                    .collect()
            })
            .unwrap_or_default();
        for callback in callbacks {
            callback(event.clone());
        }
    }

    /// Replace the canned workspace so a later open sees the new tip.
    pub fn set_workspace(&self, workspace: WorkspaceSummary) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state
            .workspaces
            .insert(workspace.project_id.clone(), workspace);
    }

    fn from_parts(
        env: Environment,
        projects: Vec<ProjectSummary>,
        workspace: Option<WorkspaceSummary>,
        worktree: Option<WorktreeView>,
        commits: HashMap<String, CommitDetails>,
        diffs: HashMap<String, TextDiff>,
        list_reason: Option<String>,
    ) -> Self {
        let mut workspaces = HashMap::new();
        let mut worktrees = HashMap::new();
        let mut remotes = HashMap::new();
        let mut catalog = HashMap::new();
        let mut reviews = HashMap::new();
        let mut next_review = 1;
        if let Some(workspace) = workspace {
            if workspace.project_id == "demo-project" {
                catalog.insert(workspace.project_id.clone(), demo_catalog());
                reviews.insert(workspace.project_id.clone(), forge::demo_reviews());
                next_review = 132;
            }
            remotes.insert(workspace.project_id.clone(), FakeRemote::demo());
            if let Some(worktree) = worktree {
                worktrees.insert(workspace.project_id.clone(), worktree);
            }
            workspaces.insert(workspace.project_id.clone(), workspace);
        }
        Self {
            state: Mutex::new(FakeState {
                env,
                projects,
                workspaces,
                worktrees,
                commits,
                diffs,
                list_reason,
                next_sha: 1,
                history: HashMap::new(),
                remotes,
                askpass: Vec::new(),
                catalog,
                edits: HashMap::new(),
                reviews,
                next_review,
                global: GlobalSettings::defaults(),
            }),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            askpass_watchers: Arc::new(Mutex::new(Vec::new())),
            tokens: AtomicU64::new(1),
            latency_ms: AtomicU64::new(latency_from_env()),
            fail_ops: Mutex::new(HashSet::new()),
        }
    }

    /// Sleep and maybe fail once. Call this before taking `state`.
    fn gate(&self, op: &str) -> Result<()> {
        let armed = {
            let mut fails = self
                .fail_ops
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            fails.remove(op)
        };
        if armed {
            return Err(BridgeError::not_ready(forced_fail(op)));
        }
        let ms = self.latency_ms.load(Ordering::Relaxed);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
        Ok(())
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn publish(&self, id: &str) {
        let view = self.locked().worktrees.get(id).cloned();
        if let Some(view) = view {
            self.trigger(id, BridgeEvent::Worktree(view));
        }
        self.trigger(id, BridgeEvent::WorkspaceActivity);
    }

    /// Write `body` into `path` for the open edit. Save applies it. Abort drops it.
    pub fn edit_file(&self, id: &str, path: &str, body: &str) -> Result<()> {
        let mut state = self.locked();
        ensure_project(&state, id)?;
        let edit = state.edits.get_mut(id).ok_or_else(|| {
            BridgeError::not_ready("Enter edit mode before changing a file here.")
        })?;
        edit.files.insert(path.to_string(), body.to_string());
        Ok(())
    }

    fn publish_head(&self, id: &str, editing: bool) {
        self.trigger(
            id,
            BridgeEvent::GitHead {
                head: Some(
                    if editing {
                        "gitbutler/edit"
                    } else {
                        "gitbutler/workspace"
                    }
                    .to_string(),
                ),
                mode: if editing { "edit" } else { "workspace" }.to_string(),
            },
        );
    }

    fn notify_askpass(&self, request: AskpassRequest) {
        let callbacks: Vec<_> = self
            .askpass_watchers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .map(|(_, callback)| Arc::clone(callback))
            .collect();
        for callback in callbacks {
            callback(request.clone());
        }
    }
}

fn fake_upstream(seed: &str, title: &str) -> FakeUpstream {
    let (sha, short_sha) = sha(seed);
    FakeUpstream {
        sha,
        short_sha,
        title: title.to_string(),
        author: "Ada Lovelace <ada@example.com>".to_string(),
    }
}

fn upstream_commit(commit: &FakeUpstream) -> UpstreamCommit {
    UpstreamCommit {
        sha: commit.sha.clone(),
        short_sha: commit.short_sha.clone(),
        title: commit.title.clone(),
        author: commit.author.clone(),
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

fn fake_base(state: &FakeState, id: &str) -> Result<Option<BaseBranchData>> {
    ensure_project(state, id)?;
    let workspace = state.workspaces.get(id).expect("project checked");
    let remote = state.remotes.get(id);
    let target = workspace
        .target_branch
        .clone()
        .unwrap_or_else(|| "refs/remotes/origin/main".to_string());
    let branch_name = target
        .strip_prefix("refs/remotes/")
        .unwrap_or(&target)
        .to_string();
    let short_name = branch_name.rsplit('/').next().unwrap_or("main").to_string();
    let fetched = remote
        .map(|remote| remote.fetched.clone())
        .unwrap_or_default();
    let push_remote = remote
        .map(|remote| remote.push_remote.clone())
        .unwrap_or_else(|| "origin".to_string());
    Ok(Some(BaseBranchData {
        branch_name,
        short_name,
        remote_name: "origin".to_string(),
        remote_url: "file://demo/remote.git".to_string(),
        push_remote_name: push_remote,
        push_remote_url: "file://demo/remote.git".to_string(),
        base_sha: "b".repeat(40),
        current_sha: fetched
            .first()
            .map(|commit| commit.sha.clone())
            .unwrap_or_else(|| "b".repeat(40)),
        behind: fetched.len(),
        upstream_commits: fetched.iter().map(upstream_commit).collect(),
        last_fetched_ms: remote.and_then(|remote| remote.last_fetched_ms),
        conflicted: false,
    }))
}

fn fake_status(state: &FakeState, id: &str) -> Result<FetchStatus> {
    let base = fake_base(state, id)?.unwrap_or(BaseBranchData {
        branch_name: "origin/main".to_string(),
        short_name: "main".to_string(),
        remote_name: "origin".to_string(),
        remote_url: String::new(),
        push_remote_name: "origin".to_string(),
        push_remote_url: String::new(),
        base_sha: "b".repeat(40),
        current_sha: "b".repeat(40),
        behind: 0,
        upstream_commits: Vec::new(),
        last_fetched_ms: None,
        conflicted: false,
    });
    let remote = state.remotes.get(id);
    Ok(FetchStatus {
        summary: FetchStatus::summary_for(base.behind),
        upstream_count: base.behind,
        last_fetched_ms: remote.and_then(|remote| remote.last_fetched_ms),
        last_attempted_ms: remote.and_then(|remote| remote.last_attempted_ms),
        last_error: remote.and_then(|remote| remote.last_error.clone()),
        commits: base.upstream_commits,
    })
}

fn lane_resolutions(
    state: &FakeState,
    id: &str,
    strategy: UpstreamStrategy,
) -> Vec<LaneResolution> {
    let Some(workspace) = state.workspaces.get(id) else {
        return Vec::new();
    };
    workspace
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .map(|branch| LaneResolution {
            lane: branch.name.clone(),
            strategy,
            resolution: if branch.commits.iter().any(|commit| commit.conflicted) {
                IntegrateResolution::Conflicted
            } else {
                IntegrateResolution::Clear
            },
            commits: branch
                .commits
                .iter()
                .map(|commit| commit.title.clone())
                .collect(),
        })
        .collect()
}

fn ensure_project(state: &FakeState, id: &str) -> Result<()> {
    if state.env.compat != Compat::Match {
        return Err(crate::compat_error(&state.env));
    }
    if !state.workspaces.contains_key(id) {
        return Err(BridgeError::not_found(format!("No project with id {id}")));
    }
    Ok(())
}

fn reviews_of(state: &FakeState, id: &str) -> Result<ForgeStatus> {
    ensure_project(state, id)?;
    let mut reviews = state.reviews.get(id).cloned().unwrap_or_default();
    reviews.sort_by_key(|review| review.number);
    Ok(ForgeStatus {
        connected: true,
        detail: String::new(),
        reviews,
    })
}

fn review_of(state: &FakeState, id: &str, branch: &str) -> Result<Option<Review>> {
    ensure_project(state, id)?;
    let reviews = state.reviews.get(id).map(Vec::as_slice).unwrap_or(&[]);
    Ok(forge::find_review(reviews, branch))
}

fn require_local_branch(state: &FakeState, id: &str, branch: &str) -> Result<()> {
    let applied = state.workspaces.get(id).is_some_and(|workspace| {
        workspace
            .stacks
            .iter()
            .any(|stack| stack.branches.iter().any(|item| item.name == branch))
    });
    if applied {
        return Ok(());
    }
    let Some(row) = state.catalog.get(id).and_then(|rows| {
        rows.iter()
            .find(|row| row.name == branch || row.ref_name == format!("refs/heads/{branch}"))
    }) else {
        return Err(BridgeError::not_found(format!("No branch {branch}.")));
    };
    match row.place {
        BranchPlace::Remote => Err(BridgeError::not_ready(
            "Only a local branch can be published.",
        )),
        BranchPlace::Local | BranchPlace::Both => Ok(()),
    }
}

fn publish_of(state: &mut FakeState, id: &str, branch: &str) -> Result<Review> {
    ensure_project(state, id)?;
    let branch = forge::normalize_branch(branch);
    if branch.is_empty() {
        return Err(BridgeError::not_ready(
            "Choose a branch before publishing a pull request.",
        ));
    }
    require_local_branch(state, id, &branch)?;
    let mut next = state.next_review;
    let review = {
        let reviews = state.reviews.entry(id.to_string()).or_default();
        forge::insert_draft(reviews, &mut next, &branch)
    };
    state.next_review = next;
    Ok(review)
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

fn fresh_sha(state: &mut FakeState) -> (String, String) {
    state.next_sha += 1;
    let full = format!("{:040x}", state.next_sha);
    let short = full[..7].to_string();
    (full, short)
}

fn capture(state: &FakeState, id: &str, operation: &str, title: &str) -> FakeImage {
    let n = state.next_sha + 1;
    FakeImage {
        info: SnapshotInfo {
            sha: format!("{n:040x}"),
            created_at_ms: i128::from(n) * 1000,
            operation: operation.to_string(),
            title: title.to_string(),
            restorable: operation != "RestoreFromSnapshotViaUndo"
                && operation != "RestoreFromSnapshotViaRedo",
        },
        workspace: state
            .workspaces
            .get(id)
            .cloned()
            .unwrap_or_else(|| WorkspaceSummary {
                project_id: id.to_string(),
                target_branch: None,
                stacks: Vec::new(),
            }),
        worktree: state.worktrees.get(id).cloned().unwrap_or(WorktreeView {
            files: Vec::new(),
            changed_paths: Vec::new(),
        }),
        commits: state.commits.clone(),
        diffs: state.diffs.clone(),
        catalog: state.catalog.get(id).cloned().unwrap_or_default(),
        edit: state.edits.get(id).cloned(),
    }
}

fn remember(state: &mut FakeState, id: &str, operation: &str, title: &str) {
    let image = capture(state, id, operation, title);
    state.next_sha += 1;
    state.history.entry(id.to_string()).or_default().push(image);
}

fn apply_image(state: &mut FakeState, id: &str, image: &FakeImage) {
    state
        .workspaces
        .insert(id.to_string(), image.workspace.clone());
    state
        .worktrees
        .insert(id.to_string(), image.worktree.clone());
    state.commits.clone_from(&image.commits);
    state.diffs.clone_from(&image.diffs);
    state.catalog.insert(id.to_string(), image.catalog.clone());
    match &image.edit {
        Some(edit) => {
            state.edits.insert(id.to_string(), edit.clone());
        }
        None => {
            state.edits.remove(id);
        }
    }
}

fn refuse_edit(state: &FakeState, id: &str) -> Result<()> {
    if state.edits.contains_key(id) {
        Err(BridgeError::not_ready(EDIT_MODE_MESSAGE))
    } else {
        Ok(())
    }
}

fn workspace_of(state: &FakeState, id: &str) -> WorkspaceSummary {
    state.workspaces.get(id).cloned().expect("project checked")
}

fn branch_mut<'a>(
    workspace: &'a mut WorkspaceSummary,
    name: &str,
) -> Result<&'a mut BranchSummary> {
    workspace
        .stacks
        .iter_mut()
        .find_map(|stack| stack.branches.iter_mut().find(|branch| branch.name == name))
        .ok_or_else(|| BridgeError::not_found(format!("No lane named {name}")))
}

fn refresh_tip(branch: &mut BranchSummary) {
    branch.tip_sha = branch
        .commits
        .first()
        .map(|commit| commit.short_sha.clone())
        .unwrap_or_default();
    branch.tip_full = branch
        .commits
        .first()
        .map(|commit| commit.sha.clone())
        .unwrap_or_default();
}

struct Located {
    lane: String,
    index: usize,
}

fn locate(workspace: &WorkspaceSummary, sha: &str) -> Result<Located> {
    for stack in &workspace.stacks {
        for branch in &stack.branches {
            if let Some(index) = branch
                .commits
                .iter()
                .position(|commit| commit.sha == sha || commit.short_sha == sha)
            {
                return Ok(Located {
                    lane: branch.name.clone(),
                    index,
                });
            }
        }
    }
    Err(BridgeError::not_found(format!("No commit {sha}")))
}

fn selection_ready(worktree: &WorktreeView, selections: &[SelectionSpec]) -> Result<()> {
    for spec in selections {
        let Some(file) = worktree.files.iter().find(|file| file.path == spec.path) else {
            return Err(BridgeError::not_found(format!(
                "No uncommitted change for {}",
                spec.path
            )));
        };
        if let Some(hunks) = &spec.hunks {
            for hunk in hunks {
                let header = hunk.header();
                if !file.hunks.iter().any(|have| have.header == header) {
                    return Err(BridgeError::not_found(format!(
                        "That hunk is not in {}",
                        spec.path
                    )));
                }
            }
        }
    }
    Ok(())
}

fn take_files(worktree: &mut WorktreeView, selections: &[SelectionSpec]) -> Vec<CommitFile> {
    let mut taken_files = Vec::new();
    for spec in selections {
        let Some(index) = worktree
            .files
            .iter()
            .position(|file| file.path == spec.path)
        else {
            continue;
        };
        let headers = spec
            .hunks
            .as_ref()
            .map(|hunks| hunks.iter().map(HunkRef::header).collect::<Vec<_>>());
        let (path, status, additions, deletions) = {
            let file = &mut worktree.files[index];
            match &headers {
                None => {
                    file.hunks.clear();
                }
                Some(headers) => {
                    file.hunks
                        .retain(|hunk| !headers.iter().any(|header| header == &hunk.header));
                }
            }
            (
                file.path.clone(),
                file.status,
                file.additions,
                file.deletions,
            )
        };
        if worktree.files[index].hunks.is_empty() {
            worktree.files.remove(index);
        }
        taken_files.push(CommitFile {
            path,
            status,
            additions,
            deletions,
        });
    }
    taken_files
}

fn make_commit(
    state: &mut FakeState,
    message: &str,
    files: Vec<CommitFile>,
    parent: Option<String>,
) -> (CommitSummary, CommitDetails) {
    let (full, short) = fresh_sha(state);
    let title = message.lines().next().unwrap_or("").trim().to_string();
    let when = ago(0);
    let author = "Gb Gpui <gb-gpui@example.com>".to_string();
    let summary = CommitSummary {
        sha: full.clone(),
        short_sha: short.clone(),
        title,
        author: author.clone(),
        authored_at: when,
        conflicted: false,
        pushed: false,
    };
    let details = CommitDetails {
        sha: full,
        short_sha: short,
        message: message.to_string(),
        author,
        committer: summary.author.clone(),
        authored_at: when,
        committed_at: when,
        parents: parent.into_iter().collect(),
        files,
    };
    (summary, details)
}

fn insert_on_branch(branch: &mut BranchSummary, index: usize, summary: CommitSummary) {
    let index = index.min(branch.commits.len());
    branch.commits.insert(index, summary);
    refresh_tip(branch);
}

fn anchor_index(branch: &BranchSummary, anchor: &Anchor) -> Result<usize> {
    match anchor {
        Anchor::Lane(name) if name == &branch.name => Ok(0),
        Anchor::Lane(name) => Err(BridgeError::not_found(format!("No lane named {name}"))),
        Anchor::Commit { sha, below } => {
            let position = branch
                .commits
                .iter()
                .position(|commit| commit.sha == *sha || commit.short_sha == *sha)
                .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
            Ok(if *below { position + 1 } else { position })
        }
    }
}

fn destination_branch<'a>(
    workspace: &'a mut WorkspaceSummary,
    anchor: &Anchor,
) -> Result<(&'a mut BranchSummary, usize)> {
    match anchor {
        Anchor::Lane(name) => {
            let branch = branch_mut(workspace, name)?;
            Ok((branch, 0))
        }
        Anchor::Commit { sha, .. } => {
            let located = locate(workspace, sha)?;
            let branch = branch_mut(workspace, &located.lane)?;
            let index = anchor_index(branch, anchor)?;
            Ok((branch, index))
        }
    }
}

fn worktree_file(file: &CommitFile, lane: Option<&str>) -> FileChange {
    FileChange {
        path: file.path.clone(),
        old_path: None,
        status: file.status,
        additions: file.additions,
        deletions: file.deletions,
        hunks: vec![AssignedHunk {
            header: "@@ -1,1 +1,1 @@".to_string(),
            lane: lane.map(str::to_string),
        }],
    }
}

fn put_worktree_file(worktree: &mut WorktreeView, file: FileChange) {
    if let Some(existing) = worktree
        .files
        .iter_mut()
        .find(|have| have.path == file.path)
    {
        existing.hunks.extend(file.hunks);
        return;
    }
    worktree.files.push(file);
}

fn commit_result(state: &FakeState, id: &str, new_sha: Option<String>) -> CommitResult {
    CommitResult {
        workspace: workspace_of(state, id),
        new_sha,
        rejected: Vec::new(),
    }
}

fn assign_in(
    state: &mut FakeState,
    id: &str,
    selections: &[SelectionSpec],
    lane: Option<&str>,
) -> Result<WorkspaceSummary> {
    require_selection(selections)?;
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    if let Some(name) = lane {
        branch_mut(state.workspaces.get_mut(id).expect("checked"), name)?;
    }
    let worktree = state
        .worktrees
        .get(id)
        .ok_or_else(|| BridgeError::not_found(format!("No worktree for project {id}")))?;
    selection_ready(worktree, selections)?;
    remember(state, id, "MoveHunk", "Moved hunk");
    let worktree = state.worktrees.get_mut(id).expect("checked");
    for spec in selections {
        let file = worktree
            .files
            .iter_mut()
            .find(|file| file.path == spec.path)
            .expect("checked");
        let headers = spec
            .hunks
            .as_ref()
            .map(|hunks| hunks.iter().map(HunkRef::header).collect::<Vec<_>>());
        for hunk in &mut file.hunks {
            let hit = match &headers {
                None => true,
                Some(headers) => headers.iter().any(|header| header == &hunk.header),
            };
            if hit {
                hunk.lane = lane.map(str::to_string);
            }
        }
    }
    Ok(workspace_of(state, id))
}

fn discard_in(
    state: &mut FakeState,
    id: &str,
    selections: &[SelectionSpec],
) -> Result<DiscardResult> {
    require_selection(selections)?;
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let worktree = state
        .worktrees
        .get(id)
        .ok_or_else(|| BridgeError::not_found(format!("No worktree for project {id}")))?;
    selection_ready(worktree, selections)?;
    remember(state, id, "DiscardChanges", "Discarded changes");
    let worktree = state.worktrees.get_mut(id).expect("checked");
    for spec in selections.iter().rev() {
        let Some(index) = worktree
            .files
            .iter()
            .position(|file| file.path == spec.path)
        else {
            continue;
        };
        match &spec.hunks {
            None => {
                worktree.files.remove(index);
            }
            Some(hunks) => {
                let headers: Vec<_> = hunks.iter().map(HunkRef::header).collect();
                worktree.files[index]
                    .hunks
                    .retain(|hunk| !headers.iter().any(|header| header == &hunk.header));
                if worktree.files[index].hunks.is_empty() {
                    worktree.files.remove(index);
                }
            }
        }
    }
    Ok(DiscardResult {
        workspace: workspace_of(state, id),
        refused: Vec::new(),
    })
}

fn create_lane_in(state: &mut FakeState, id: &str, name: &str) -> Result<LaneCreated> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    if name.trim().is_empty() || name.contains(char::is_whitespace) {
        return Err(BridgeError::not_ready(format!(
            "'{name}' is not a branch name"
        )));
    }
    if state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .any(|stack| stack.names().any(|have| have == name))
    {
        return Err(BridgeError::not_ready(format!(
            "Lane {name} already exists."
        )));
    }
    remember(state, id, "CreateBranch", "Created branch");
    state
        .workspaces
        .get_mut(id)
        .expect("checked")
        .stacks
        .push(StackSummary {
            branches: vec![BranchSummary {
                name: name.to_string(),
                tip_sha: String::new(),
                tip_full: String::new(),
                commits: Vec::new(),
            }],
        });
    Ok(LaneCreated {
        workspace: workspace_of(state, id),
        name: name.to_string(),
    })
}

fn commit_create_in(
    state: &mut FakeState,
    id: &str,
    anchor: &Anchor,
    selections: &[SelectionSpec],
    message: &str,
) -> Result<CommitResult> {
    require_selection(selections)?;
    require_message(message)?;
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let worktree = state
        .worktrees
        .get(id)
        .ok_or_else(|| BridgeError::not_found(format!("No worktree for project {id}")))?;
    selection_ready(worktree, selections)?;
    {
        let workspace = state.workspaces.get_mut(id).expect("checked");
        let _ = destination_branch(workspace, anchor)?;
    }
    remember(state, id, "CreateCommit", "Created commit");
    let files = take_files(state.worktrees.get_mut(id).expect("checked"), selections);
    let parent = {
        let (branch, index) =
            destination_branch(state.workspaces.get_mut(id).expect("checked"), anchor)?;
        branch.commits.get(index).map(|commit| commit.sha.clone())
    };
    let (summary, details) = make_commit(state, message, files, parent);
    let sha = summary.sha.clone();
    state.commits.insert(sha.clone(), details);
    let (branch, index) =
        destination_branch(state.workspaces.get_mut(id).expect("checked"), anchor)?;
    insert_on_branch(branch, index, summary);
    Ok(commit_result(state, id, Some(sha)))
}

fn amend_in(
    state: &mut FakeState,
    id: &str,
    sha: &str,
    selections: &[SelectionSpec],
) -> Result<CommitResult> {
    require_selection(selections)?;
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let located = locate(state.workspaces.get(id).expect("checked"), sha)?;
    let worktree = state
        .worktrees
        .get(id)
        .ok_or_else(|| BridgeError::not_found(format!("No worktree for project {id}")))?;
    selection_ready(worktree, selections)?;
    remember(state, id, "AmendCommit", "Amended commit");
    let extra = take_files(state.worktrees.get_mut(id).expect("checked"), selections);
    let old = state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == located.lane)
        .expect("lane")
        .commits[located.index]
        .sha
        .clone();
    let previous = state
        .commits
        .get(&old)
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
    let mut files = previous.files.clone();
    files.extend(extra);
    let (summary, mut details) = make_commit(
        state,
        &previous.message,
        files,
        previous.parents.first().cloned(),
    );
    details.message = previous.message;
    let new_sha = summary.sha.clone();
    state.commits.remove(&old);
    state.commits.insert(new_sha.clone(), details);
    let branch = branch_mut(
        state.workspaces.get_mut(id).expect("checked"),
        &located.lane,
    )?;
    branch.commits[located.index] = summary;
    refresh_tip(branch);
    Ok(commit_result(state, id, Some(new_sha)))
}

fn reword_in(state: &mut FakeState, id: &str, sha: &str, message: &str) -> Result<CommitResult> {
    require_message(message)?;
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let located = locate(state.workspaces.get(id).expect("checked"), sha)?;
    let old = state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == located.lane)
        .expect("lane")
        .commits[located.index]
        .sha
        .clone();
    let previous = state
        .commits
        .get(&old)
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
    remember(state, id, "UpdateCommitMessage", "Updated commit message");
    let (summary, mut details) = make_commit(
        state,
        message,
        previous.files,
        previous.parents.first().cloned(),
    );
    details.message = message.to_string();
    let new_sha = summary.sha.clone();
    state.commits.remove(&old);
    state.commits.insert(new_sha.clone(), details);
    let branch = branch_mut(
        state.workspaces.get_mut(id).expect("checked"),
        &located.lane,
    )?;
    branch.commits[located.index] = summary;
    refresh_tip(branch);
    Ok(commit_result(state, id, Some(new_sha)))
}

fn squash_in(
    state: &mut FakeState,
    id: &str,
    subject_shas: &[String],
    target_sha: &str,
) -> Result<CommitResult> {
    if subject_shas.is_empty() {
        return Err(BridgeError::not_ready("Choose a commit to squash."));
    }
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let target = locate(state.workspaces.get(id).expect("checked"), target_sha)?;
    let subjects: Vec<Located> = subject_shas
        .iter()
        .map(|sha| locate(state.workspaces.get(id).expect("checked"), sha))
        .collect::<Result<Vec<_>>>()?;
    remember(state, id, "SquashCommit", "Squashed commit");
    let target_sha_full = full_sha(state, id, &target);
    let mut target_details = state
        .commits
        .get(&target_sha_full)
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {target_sha}")))?;
    let mut message = target_details.message.clone();
    for subject in &subjects {
        let sha = full_sha(state, id, subject);
        let details = state
            .commits
            .get(&sha)
            .cloned()
            .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
        message.push_str("\n\n");
        message.push_str(&details.message);
        target_details.files.extend(details.files);
    }
    let parent = target_details.parents.first().cloned();
    let files = target_details.files.clone();
    let (summary, mut details) = make_commit(state, &message, files, parent);
    details.message = message;
    let new_sha = summary.sha.clone();
    let subject_full: Vec<String> = subjects
        .iter()
        .map(|subject| full_sha(state, id, subject))
        .collect();
    for sha in &subject_full {
        remove_commit(state, id, sha);
    }
    let target = locate(state.workspaces.get(id).expect("checked"), &target_sha_full)?;
    state.commits.remove(&target_sha_full);
    state.commits.insert(new_sha.clone(), details);
    let branch = branch_mut(state.workspaces.get_mut(id).expect("checked"), &target.lane)?;
    branch.commits[target.index] = summary;
    refresh_tip(branch);
    Ok(commit_result(state, id, Some(new_sha)))
}

fn remove_commit(state: &mut FakeState, id: &str, sha: &str) -> Option<CommitDetails> {
    let located = locate(state.workspaces.get(id)?, sha).ok()?;
    let summary = {
        let branch = branch_mut(state.workspaces.get_mut(id)?, &located.lane).ok()?;
        if located.index >= branch.commits.len() {
            return None;
        }
        let summary = branch.commits.remove(located.index);
        refresh_tip(branch);
        summary
    };
    state.commits.remove(&summary.sha)
}

fn full_sha(state: &FakeState, id: &str, located: &Located) -> String {
    state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == located.lane)
        .expect("lane")
        .commits[located.index]
        .sha
        .clone()
}

fn uncommit_in(
    state: &mut FakeState,
    id: &str,
    shas: &[String],
    lane: Option<&str>,
) -> Result<CommitResult> {
    if shas.is_empty() {
        return Err(BridgeError::not_ready("Choose a commit to uncommit."));
    }
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    if let Some(name) = lane {
        branch_mut(state.workspaces.get_mut(id).expect("checked"), name)?;
    }
    let located: Vec<Located> = shas
        .iter()
        .map(|sha| locate(state.workspaces.get(id).expect("checked"), sha))
        .collect::<Result<Vec<_>>>()?;
    remember(state, id, "UndoCommit", "Undid commit");
    let mut restored = Vec::new();
    let fulls: Vec<String> = located
        .iter()
        .map(|place| full_sha(state, id, place))
        .collect();
    for sha in fulls {
        if let Some(details) = remove_commit(state, id, &sha) {
            restored.extend(details.files);
        }
    }
    let worktree = state
        .worktrees
        .entry(id.to_string())
        .or_insert(WorktreeView {
            files: Vec::new(),
            changed_paths: Vec::new(),
        });
    for file in restored {
        put_worktree_file(worktree, worktree_file(&file, lane));
    }
    Ok(commit_result(state, id, None))
}

fn uncommit_changes_in(
    state: &mut FakeState,
    id: &str,
    sha: &str,
    selections: &[SelectionSpec],
    lane: Option<&str>,
) -> Result<CommitResult> {
    require_selection(selections)?;
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    if let Some(name) = lane {
        branch_mut(state.workspaces.get_mut(id).expect("checked"), name)?;
    }
    let located = locate(state.workspaces.get(id).expect("checked"), sha)?;
    let full = full_sha(state, id, &located);
    let details = state
        .commits
        .get(&full)
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
    for spec in selections {
        if !details.files.iter().any(|file| file.path == spec.path) {
            return Err(BridgeError::not_found(format!(
                "{} is not part of commit {sha}",
                spec.path
            )));
        }
    }
    remember(state, id, "UndoCommit", "Undid commit");
    let mut moving = Vec::new();
    let mut staying = Vec::new();
    for file in details.files {
        if selections.iter().any(|spec| spec.path == file.path) {
            moving.push(file);
        } else {
            staying.push(file);
        }
    }
    let (summary, mut rewritten) = make_commit(
        state,
        &details.message,
        staying,
        details.parents.first().cloned(),
    );
    rewritten.message = details.message;
    let new_sha = summary.sha.clone();
    state.commits.remove(&full);
    state.commits.insert(new_sha.clone(), rewritten);
    let branch = branch_mut(
        state.workspaces.get_mut(id).expect("checked"),
        &located.lane,
    )?;
    branch.commits[located.index] = summary;
    refresh_tip(branch);
    let worktree = state
        .worktrees
        .entry(id.to_string())
        .or_insert(WorktreeView {
            files: Vec::new(),
            changed_paths: Vec::new(),
        });
    for file in moving {
        put_worktree_file(worktree, worktree_file(&file, lane));
    }
    Ok(commit_result(state, id, Some(new_sha)))
}

fn move_commit_in(
    state: &mut FakeState,
    id: &str,
    shas: &[String],
    anchor: &Anchor,
) -> Result<CommitResult> {
    if shas.is_empty() {
        return Err(BridgeError::not_ready("Choose a commit to move."));
    }
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let located: Vec<Located> = shas
        .iter()
        .map(|sha| locate(state.workspaces.get(id).expect("checked"), sha))
        .collect::<Result<Vec<_>>>()?;
    {
        let workspace = state.workspaces.get_mut(id).expect("checked");
        let _ = destination_branch(workspace, anchor)?;
    }
    remember(state, id, "MoveCommit", "Moved commit");
    let fulls: Vec<String> = located
        .iter()
        .map(|place| full_sha(state, id, place))
        .collect();
    let mut moving = Vec::new();
    for sha in &fulls {
        let place = locate(state.workspaces.get(id).expect("checked"), sha)?;
        let branch = branch_mut(state.workspaces.get_mut(id).expect("checked"), &place.lane)?;
        let summary = branch.commits.remove(place.index);
        refresh_tip(branch);
        moving.push(summary);
    }
    let (branch, mut index) =
        destination_branch(state.workspaces.get_mut(id).expect("checked"), anchor)?;
    for summary in moving {
        insert_on_branch(branch, index, summary);
        index += 1;
    }
    Ok(commit_result(state, id, None))
}

fn insert_blank_in(state: &mut FakeState, id: &str, anchor: &Anchor) -> Result<CommitResult> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    {
        let workspace = state.workspaces.get_mut(id).expect("checked");
        let _ = destination_branch(workspace, anchor)?;
    }
    remember(state, id, "InsertBlankCommit", "Inserted blank commit");
    let parent = {
        let (branch, index) =
            destination_branch(state.workspaces.get_mut(id).expect("checked"), anchor)?;
        branch.commits.get(index).map(|commit| commit.sha.clone())
    };
    let (summary, details) = make_commit(state, "(empty commit)", Vec::new(), parent);
    let sha = summary.sha.clone();
    state.commits.insert(sha.clone(), details);
    let (branch, index) =
        destination_branch(state.workspaces.get_mut(id).expect("checked"), anchor)?;
    insert_on_branch(branch, index, summary);
    Ok(commit_result(state, id, Some(sha)))
}

fn move_changes_in(
    state: &mut FakeState,
    id: &str,
    source_sha: &str,
    destination_sha: &str,
    selections: &[SelectionSpec],
) -> Result<CommitResult> {
    require_selection(selections)?;
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let source_at = locate(state.workspaces.get(id).expect("checked"), source_sha)?;
    let destination_at = locate(state.workspaces.get(id).expect("checked"), destination_sha)?;
    let source_full = full_sha(state, id, &source_at);
    let destination_full = full_sha(state, id, &destination_at);
    let source_details = state
        .commits
        .get(&source_full)
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {source_sha}")))?;
    let destination_details = state
        .commits
        .get(&destination_full)
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {destination_sha}")))?;
    for spec in selections {
        if !source_details
            .files
            .iter()
            .any(|file| file.path == spec.path)
        {
            return Err(BridgeError::not_found(format!(
                "{} is not part of commit {source_sha}",
                spec.path
            )));
        }
    }
    remember(state, id, "MoveCommitFile", "Moved changes");
    let mut moving = Vec::new();
    let mut staying = Vec::new();
    for file in source_details.files {
        if selections.iter().any(|spec| spec.path == file.path) {
            moving.push(file);
        } else {
            staying.push(file);
        }
    }
    let (source_summary, mut source_rewritten) = make_commit(
        state,
        &source_details.message,
        staying,
        source_details.parents.first().cloned(),
    );
    source_rewritten.message = source_details.message;
    let mut dest_files = destination_details.files.clone();
    dest_files.extend(moving);
    let (dest_summary, mut dest_rewritten) = make_commit(
        state,
        &destination_details.message,
        dest_files,
        destination_details.parents.first().cloned(),
    );
    dest_rewritten.message = destination_details.message;
    state.commits.remove(&source_full);
    state.commits.remove(&destination_full);
    state
        .commits
        .insert(source_summary.sha.clone(), source_rewritten);
    state
        .commits
        .insert(dest_summary.sha.clone(), dest_rewritten);
    {
        let branch = branch_mut(
            state.workspaces.get_mut(id).expect("checked"),
            &source_at.lane,
        )?;
        branch.commits[source_at.index] = source_summary;
        refresh_tip(branch);
    }
    let destination_at = locate(
        state.workspaces.get(id).expect("checked"),
        &destination_full,
    )
    .unwrap_or(destination_at);
    let branch = branch_mut(
        state.workspaces.get_mut(id).expect("checked"),
        &destination_at.lane,
    )?;
    branch.commits[destination_at.index] = dest_summary;
    refresh_tip(branch);
    Ok(commit_result(state, id, None))
}

fn snapshots_in(state: &FakeState, id: &str, limit: usize) -> Result<Vec<SnapshotInfo>> {
    ensure_project(state, id)?;
    Ok(state
        .history
        .get(id)
        .map(|history| {
            history
                .iter()
                .rev()
                .take(limit)
                .map(|image| image.info.clone())
                .collect()
        })
        .unwrap_or_default())
}

fn restore_in(state: &mut FakeState, id: &str, sha: &str) -> Result<UndoResult> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let index = state
        .history
        .get(id)
        .and_then(|history| history.iter().position(|image| image.info.sha == sha))
        .ok_or_else(|| BridgeError::not_found(format!("No snapshot {sha}")))?;
    let image = state.history.get(id).expect("checked")[index].clone();
    // The restore itself is a snapshot, so undo puts the mutated project back.
    remember(state, id, "RestoreFromSnapshot", "Restored snapshot");
    apply_image(state, id, &image);
    Ok(UndoResult {
        workspace: workspace_of(state, id),
        restored_sha: sha.to_string(),
    })
}

fn undo_in(state: &mut FakeState, id: &str) -> Result<UndoResult> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let image = state
        .history
        .get_mut(id)
        .and_then(|history| history.pop())
        .ok_or_else(|| {
            BridgeError::not_found("There is nothing to undo.")
                .with_hint("Undo restores the previous snapshot. Nothing has been recorded yet.")
        })?;
    let restored = image.info.sha.clone();
    apply_image(state, id, &image);
    Ok(UndoResult {
        workspace: workspace_of(state, id),
        restored_sha: restored,
    })
}

fn hook_in(state: &FakeState, id: &str) -> Result<HookReport> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    Ok(HookReport {
        status: HookStatus::NotConfigured,
    })
}

fn demo_catalog() -> Vec<CatalogBranch> {
    let (topic_sha, topic_short) = sha("e11e11e");
    let (remote_sha, remote_short) = sha("f11f11f");
    let author = "Ada Lovelace <ada@example.com>".to_string();
    vec![
        CatalogBranch {
            name: "topic".to_string(),
            ref_name: "refs/heads/topic".to_string(),
            place: BranchPlace::Both,
            remotes: vec!["refs/remotes/origin/topic".to_string()],
            tip_sha: topic_sha.clone(),
            tip_short: topic_short.clone(),
            last_commit: "Start the topic".to_string(),
            author: author.clone(),
            updated_at_ms: Some(1_700_000_000_000),
            ahead: Some(1),
            behind: Some(0),
            commits: vec![CommitSummary {
                sha: topic_sha,
                short_sha: topic_short,
                title: "Start the topic".to_string(),
                author: author.clone(),
                authored_at: 1_700_000_000,
                conflicted: false,
                pushed: false,
            }],
        },
        CatalogBranch {
            name: "remote-only".to_string(),
            ref_name: "refs/remotes/origin/remote-only".to_string(),
            place: BranchPlace::Remote,
            remotes: vec!["refs/remotes/origin/remote-only".to_string()],
            tip_sha: remote_sha.clone(),
            tip_short: remote_short.clone(),
            last_commit: "Only on the remote".to_string(),
            author,
            updated_at_ms: Some(1_700_000_100_000),
            ahead: Some(2),
            behind: Some(1),
            commits: vec![CommitSummary {
                sha: remote_sha,
                short_sha: remote_short,
                title: "Only on the remote".to_string(),
                author: "Ada Lovelace <ada@example.com>".to_string(),
                authored_at: 1_700_000_100,
                conflicted: false,
                pushed: true,
            }],
        },
    ]
}

fn applied_rows(workspace: &WorkspaceSummary) -> Vec<BranchRow> {
    workspace
        .stacks
        .iter()
        .flat_map(|stack| {
            let stack_branches: Vec<String> = stack.names().map(str::to_string).collect();
            stack.branches.iter().map(move |branch| {
                let ahead = branch
                    .commits
                    .iter()
                    .filter(|commit| !commit.pushed)
                    .count();
                BranchRow {
                    name: branch.name.clone(),
                    ref_name: format!("refs/heads/{}", branch.name),
                    place: BranchPlace::Local,
                    remotes: Vec::new(),
                    applied: true,
                    stack: "applied".to_string(),
                    stack_branches: stack_branches.clone(),
                    tip_sha: branch.tip_full.clone(),
                    tip_short: branch.tip_sha.clone(),
                    last_commit: branch
                        .commits
                        .first()
                        .map(|commit| commit.title.clone())
                        .unwrap_or_default(),
                    author: branch
                        .commits
                        .first()
                        .map(|commit| commit.author.clone())
                        .unwrap_or_default(),
                    updated_at_ms: branch
                        .commits
                        .first()
                        .map(|commit| commit.authored_at.saturating_mul(1000)),
                    ahead: Some(ahead),
                    behind: Some(0),
                }
            })
        })
        .collect()
}

fn catalog_row(branch: &CatalogBranch) -> BranchRow {
    BranchRow {
        name: branch.name.clone(),
        ref_name: branch.ref_name.clone(),
        place: branch.place,
        remotes: branch.remotes.clone(),
        applied: false,
        stack: "unapplied".to_string(),
        stack_branches: vec![branch.name.clone()],
        tip_sha: branch.tip_sha.clone(),
        tip_short: branch.tip_short.clone(),
        last_commit: branch.last_commit.clone(),
        author: branch.author.clone(),
        updated_at_ms: branch.updated_at_ms,
        ahead: branch.ahead,
        behind: branch.behind,
    }
}

fn list_branches_in(state: &FakeState, id: &str) -> Result<Vec<BranchRow>> {
    ensure_project(state, id)?;
    let workspace = state.workspaces.get(id).expect("checked");
    let mut rows = applied_rows(workspace);
    if let Some(catalog) = state.catalog.get(id) {
        rows.extend(catalog.iter().map(catalog_row));
    }
    Ok(rows)
}

fn branch_matches(branch: &CatalogBranch, name: &str) -> bool {
    branch.ref_name == name || branch.name == name || format!("refs/heads/{}", branch.name) == name
}

fn branch_details_in(state: &FakeState, id: &str, ref_name: &str) -> Result<BranchDetails> {
    let rows = list_branches_in(state, id)?;
    let row = rows
        .iter()
        .find(|row| row.ref_name == ref_name || row.name == ref_name)
        .ok_or_else(|| BridgeError::not_found(format!("No branch {ref_name}")))?;
    let commits = if row.applied {
        state
            .workspaces
            .get(id)
            .expect("checked")
            .stacks
            .iter()
            .find_map(|stack| {
                stack
                    .branches
                    .iter()
                    .find(|branch| branch.name == row.name)
                    .map(|branch| branch.commits.clone())
            })
            .unwrap_or_default()
    } else {
        state
            .catalog
            .get(id)
            .and_then(|catalog| {
                catalog
                    .iter()
                    .find(|branch| branch_matches(branch, ref_name) || branch.name == row.name)
                    .map(|branch| branch.commits.clone())
            })
            .unwrap_or_default()
    };
    Ok(BranchDetails {
        name: row.name.clone(),
        ref_name: row.ref_name.clone(),
        applied: row.applied,
        commits,
        ahead: row.ahead,
        behind: row.behind,
    })
}

fn apply_branch_in(state: &mut FakeState, id: &str, ref_name: &str) -> Result<ApplyResult> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let wanted = ref_name.trim();
    if wanted.is_empty() {
        return Err(BridgeError::not_ready("Choose a branch."));
    }
    let already = state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .any(|stack| {
            stack.names().any(|name| {
                name == wanted || format!("refs/heads/{name}") == wanted || name == wanted
            })
        });
    if already {
        return Ok(ApplyResult {
            workspace: workspace_of(state, id),
            names: Vec::new(),
            already_applied: true,
        });
    }
    let index = state
        .catalog
        .get(id)
        .and_then(|catalog| {
            catalog
                .iter()
                .position(|branch| branch_matches(branch, wanted))
        })
        .ok_or_else(|| BridgeError::not_found(format!("No branch {wanted}")))?;
    if state.catalog.get(id).expect("checked")[index].place == BranchPlace::Remote {
        return Err(BridgeError::not_ready("That branch is only on the remote."));
    }
    let branch = state.catalog.get(id).expect("checked")[index].clone();
    let name = branch.name.clone();
    remember(state, id, "CreateBranch", "Created branch");
    state.catalog.get_mut(id).expect("checked").remove(index);
    state
        .workspaces
        .get_mut(id)
        .expect("checked")
        .stacks
        .push(StackSummary {
            branches: vec![BranchSummary {
                name: name.clone(),
                tip_sha: branch.tip_short,
                tip_full: branch.tip_sha,
                commits: branch.commits,
            }],
        });
    Ok(ApplyResult {
        workspace: workspace_of(state, id),
        names: vec![name],
        already_applied: false,
    })
}

fn unapply_branch_in(state: &mut FakeState, id: &str, name: &str) -> Result<WorkspaceSummary> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let short = name
        .trim()
        .strip_prefix("refs/heads/")
        .unwrap_or(name.trim())
        .to_string();
    let index = state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .position(|stack| stack.names().any(|have| have == short))
        .ok_or_else(|| BridgeError::not_found(format!("No lane named {short}")))?;
    remember(state, id, "UnapplyBranch", "Unapplied branch");
    let stack = state
        .workspaces
        .get_mut(id)
        .expect("checked")
        .stacks
        .remove(index);
    let catalog = state.catalog.entry(id.to_string()).or_default();
    for branch in stack.branches {
        catalog.push(CatalogBranch {
            ref_name: format!("refs/heads/{}", branch.name),
            name: branch.name.clone(),
            place: BranchPlace::Local,
            remotes: Vec::new(),
            tip_short: branch.tip_sha.clone(),
            tip_sha: branch.tip_full.clone(),
            last_commit: branch
                .commits
                .first()
                .map(|commit| commit.title.clone())
                .unwrap_or_default(),
            author: branch
                .commits
                .first()
                .map(|commit| commit.author.clone())
                .unwrap_or_default(),
            updated_at_ms: None,
            ahead: Some(
                branch
                    .commits
                    .iter()
                    .filter(|commit| !commit.pushed)
                    .count(),
            ),
            behind: Some(0),
            commits: branch.commits,
        });
    }
    Ok(workspace_of(state, id))
}

fn delete_local_branch_in(state: &mut FakeState, id: &str, name: &str) -> Result<WorkspaceSummary> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let wanted = name.trim();
    let short = wanted.strip_prefix("refs/heads/").unwrap_or(wanted);
    if state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .any(|stack| stack.names().any(|have| have == short || have == wanted))
    {
        return Err(BridgeError::not_ready(format!(
            "Unapply {short} before deleting it. It is in the workspace."
        )));
    }
    let (index, remote_only) = {
        let catalog = state
            .catalog
            .get(id)
            .ok_or_else(|| BridgeError::not_found(format!("No branch {wanted}")))?;
        let index = catalog
            .iter()
            .position(|branch| branch_matches(branch, wanted) || branch.name == short)
            .ok_or_else(|| BridgeError::not_found(format!("No branch {wanted}")))?;
        (index, catalog[index].place == BranchPlace::Remote)
    };
    if remote_only {
        return Err(BridgeError::not_ready(
            "Only a local branch can be deleted.",
        ));
    }
    remember(state, id, "DeleteBranch", "Deleted branch");
    state.catalog.get_mut(id).expect("checked").remove(index);
    Ok(workspace_of(state, id))
}

fn create_snapshot_in(state: &mut FakeState, id: &str, message: &str) -> Result<SnapshotInfo> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let title = {
        let trimmed = message.trim();
        if trimmed.is_empty() {
            "Created snapshot".to_string()
        } else {
            trimmed.to_string()
        }
    };
    remember(state, id, "OnDemandSnapshot", &title);
    Ok(state
        .history
        .get(id)
        .and_then(|history| history.last())
        .expect("snapshot was just recorded")
        .info
        .clone())
}

fn snapshot_diff_in(state: &FakeState, id: &str, sha: &str) -> Result<Vec<SnapshotChange>> {
    ensure_project(state, id)?;
    let image = state
        .history
        .get(id)
        .and_then(|history| history.iter().find(|image| image.info.sha == sha))
        .ok_or_else(|| BridgeError::not_found(format!("No snapshot {sha}")))?;
    let old: Vec<String> = image
        .worktree
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let current: Vec<String> = state
        .worktrees
        .get(id)
        .map(|view| view.files.iter().map(|file| file.path.clone()).collect())
        .unwrap_or_default();
    let mut changes = Vec::new();
    for path in &old {
        if !current.iter().any(|have| have == path) {
            changes.push(SnapshotChange {
                path: path.clone(),
                old_path: None,
                status: FileStatus::Deleted,
            });
        }
    }
    for path in &current {
        if !old.iter().any(|have| have == path) {
            changes.push(SnapshotChange {
                path: path.clone(),
                old_path: None,
                status: FileStatus::Added,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

fn edit_status_in(state: &FakeState, id: &str) -> Result<EditStatus> {
    ensure_project(state, id)?;
    Ok(match state.edits.get(id) {
        Some(edit) => EditStatus {
            editing: true,
            commit_sha: Some(edit.sha.clone()),
            stack_id: Some(edit.stack_id.clone()),
            head: Some("gitbutler/edit".to_string()),
            conflicted_files: Vec::new(),
        },
        None => EditStatus {
            editing: false,
            commit_sha: None,
            stack_id: None,
            head: Some("gitbutler/workspace".to_string()),
            conflicted_files: Vec::new(),
        },
    })
}

fn enter_edit_in(state: &mut FakeState, id: &str, sha: &str) -> Result<EditStatus> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    let located = locate(state.workspaces.get(id).expect("checked"), sha)?;
    remember(state, id, "EnterEditMode", "Entered edit mode");
    state.edits.insert(
        id.to_string(),
        FakeEdit {
            sha: sha.to_string(),
            stack_id: format!("stack-{}", located.lane),
            lane: located.lane,
            files: HashMap::new(),
        },
    );
    edit_status_in(state, id)
}

fn save_edit_in(state: &mut FakeState, id: &str) -> Result<WorkspaceSummary> {
    ensure_project(state, id)?;
    let Some(edit) = state.edits.remove(id) else {
        return Err(BridgeError::not_ready("This project is not in edit mode."));
    };
    let Some(details) = state.commits.get(&edit.sha).cloned() else {
        return Err(BridgeError::not_found(format!("No commit {}", edit.sha)));
    };
    let title = crate::first_line(&details.message);
    let mut message = details.message;
    let mut notes = Vec::new();
    for (path, body) in &edit.files {
        notes.push(format!("Edited {path}.\n{body}"));
    }
    if !notes.is_empty() {
        if !message.ends_with('\n') {
            message.push('\n');
        }
        message.push_str(&notes.join("\n"));
    }
    let (full, short) = fresh_sha(state);
    let summary = CommitSummary {
        sha: full.clone(),
        short_sha: short.clone(),
        title,
        author: details.author.clone(),
        authored_at: details.authored_at,
        conflicted: false,
        pushed: false,
    };
    let rewritten = CommitDetails {
        sha: full.clone(),
        short_sha: short,
        message,
        author: details.author,
        committer: details.committer,
        authored_at: details.authored_at,
        committed_at: details.committed_at,
        parents: details.parents,
        files: details.files,
    };
    state.commits.remove(&edit.sha);
    state.commits.insert(full, rewritten);
    let branch = branch_mut(state.workspaces.get_mut(id).expect("checked"), &edit.lane)?;
    if let Some(commit) = branch
        .commits
        .iter_mut()
        .find(|commit| commit.sha == edit.sha)
    {
        *commit = summary;
    }
    refresh_tip(branch);
    Ok(workspace_of(state, id))
}

fn abort_edit_in(state: &mut FakeState, id: &str, force: bool) -> Result<WorkspaceSummary> {
    ensure_project(state, id)?;
    let clean = state
        .edits
        .get(id)
        .ok_or_else(|| BridgeError::not_ready("This project is not in edit mode."))?
        .files
        .is_empty();
    if !clean && !force {
        return Err(BridgeError::not_ready(
            "This edit has changes. Abort it only if you mean to drop them.",
        ));
    }
    state.edits.remove(id);
    Ok(workspace_of(state, id))
}

fn conflicted_commits_in(state: &FakeState, id: &str) -> Result<Vec<ConflictedCommit>> {
    ensure_project(state, id)?;
    let mut commits = Vec::new();
    for stack in &state.workspaces.get(id).expect("checked").stacks {
        for branch in &stack.branches {
            for commit in &branch.commits {
                if !commit.conflicted {
                    continue;
                }
                commits.push(ConflictedCommit {
                    sha: commit.sha.clone(),
                    short_sha: commit.short_sha.clone(),
                    title: commit.title.clone(),
                    author: commit.author.clone(),
                    lane: branch.name.clone(),
                });
            }
        }
    }
    Ok(commits)
}

fn resolve_in(
    state: &mut FakeState,
    id: &str,
    sha: &str,
    files: &[FileChoice],
) -> Result<ResolveReport> {
    ensure_project(state, id)?;
    refuse_edit(state, id)?;
    if files.is_empty() {
        return Err(BridgeError::not_ready(
            "Choose ours or theirs for a conflicted file.",
        ));
    }
    let located = locate(state.workspaces.get(id).expect("checked"), sha)?;
    let summary = state
        .workspaces
        .get(id)
        .expect("checked")
        .stacks
        .iter()
        .find_map(|stack| {
            stack
                .branches
                .iter()
                .find(|branch| branch.name == located.lane)
                .and_then(|branch| branch.commits.get(located.index))
        })
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
    if !summary.conflicted {
        return Err(BridgeError::not_ready("That commit has no conflicts."));
    }
    let details = state
        .commits
        .get(sha)
        .cloned()
        .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
    for choice in files {
        if !details.files.iter().any(|file| file.path == choice.path) {
            return Err(BridgeError::not_found(format!(
                "No conflict in {}.",
                choice.path
            )));
        }
    }
    remember(state, id, "ResolveConflicts", "Resolved conflicts");
    let mut message = details.message;
    for choice in files {
        let side = match choice.choice {
            ConflictChoice::Ours => "ours",
            ConflictChoice::Theirs => "theirs",
        };
        message.push_str(&format!("\nResolved {} as {side}.", choice.path));
    }
    let (full, short) = fresh_sha(state);
    let rewritten_summary = CommitSummary {
        sha: full.clone(),
        short_sha: short.clone(),
        title: summary.title,
        author: summary.author,
        authored_at: summary.authored_at,
        conflicted: false,
        pushed: false,
    };
    state.commits.remove(sha);
    state.commits.insert(
        full.clone(),
        CommitDetails {
            sha: full.clone(),
            short_sha: short,
            message,
            author: details.author,
            committer: details.committer,
            authored_at: details.authored_at,
            committed_at: details.committed_at,
            parents: details.parents,
            files: details.files,
        },
    );
    let branch = branch_mut(
        state.workspaces.get_mut(id).expect("checked"),
        &located.lane,
    )?;
    branch.commits[located.index] = rewritten_summary;
    refresh_tip(branch);
    Ok(ResolveReport {
        previous_sha: sha.to_string(),
        new_sha: full,
        resolved: files.len(),
        still_conflicted: false,
        workspace: workspace_of(state, id),
    })
}

fn environment_for(compat: Compat, installed: Option<String>) -> Environment {
    Environment {
        installed,
        pinned: PINNED_VERSION.to_string(),
        app_data_dir: PathBuf::from("/demo/app-data"),
        config_dir: PathBuf::from("/demo/config"),
        compat,
    }
}

fn ago(seconds: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(1_700_000_000);
    now - seconds
}

fn sha(seed: &str) -> (String, String) {
    let mut full = String::new();
    while full.len() < 40 {
        full.push_str(seed);
    }
    full.truncate(40);
    let short = full.chars().take(7).collect();
    (full, short)
}

struct DemoCommit<'a> {
    seed: &'a str,
    title: &'a str,
    body: &'a str,
    author: &'a str,
    ago_seconds: i64,
    conflicted: bool,
    pushed: bool,
    files: Vec<CommitFile>,
}

fn commit(draft: DemoCommit<'_>) -> (CommitSummary, CommitDetails) {
    let DemoCommit {
        seed,
        title,
        body,
        author,
        ago_seconds,
        conflicted,
        pushed,
        files,
    } = draft;
    let (full, short) = sha(seed);
    let summary = CommitSummary {
        sha: full.clone(),
        short_sha: short.clone(),
        title: title.to_string(),
        author: author.to_string(),
        authored_at: ago(ago_seconds),
        conflicted,
        pushed,
    };
    let message = if body.is_empty() {
        title.to_string()
    } else {
        format!("{title}\n\n{body}")
    };
    let details = CommitDetails {
        sha: full,
        short_sha: short,
        message,
        author: author.to_string(),
        committer: author.to_string(),
        authored_at: summary.authored_at,
        committed_at: summary.authored_at,
        parents: vec![sha("parent00").0],
        files,
    };
    (summary, details)
}

fn branch(name: &str, commits: Vec<CommitSummary>) -> BranchSummary {
    BranchSummary {
        name: name.to_string(),
        tip_sha: commits
            .first()
            .map(|commit| commit.short_sha.clone())
            .unwrap_or_default(),
        tip_full: commits
            .first()
            .map(|commit| commit.sha.clone())
            .unwrap_or_default(),
        commits,
    }
}

fn file(
    path: &str,
    status: FileStatus,
    old: Option<&str>,
    additions: u32,
    deletions: u32,
    hunks: Vec<AssignedHunk>,
) -> FileChange {
    FileChange {
        path: path.to_string(),
        old_path: old.map(str::to_string),
        status,
        additions,
        deletions,
        hunks,
    }
}

fn hunk(header: &str, lane: Option<&str>) -> AssignedHunk {
    AssignedHunk {
        header: header.to_string(),
        lane: lane.map(str::to_string),
    }
}

fn text_diff(path: &str, patch: String) -> TextDiff {
    TextDiff {
        path: path.to_string(),
        patch,
        binary: false,
        too_large: false,
    }
}

fn app_patch() -> String {
    let mut patch = String::from("@@ -1,4 +1,154 @@\n fn main() {\n-    old_draw();\n");
    for index in 0..150 {
        patch.push_str(&format!("+    draw_row_{index}();\n"));
    }
    patch.push_str(" }\n@@ -40,2 +190,152 @@\n fn paint() {\n");
    for index in 0..150 {
        patch.push_str(&format!("+    paint_pass_{index}();\n"));
    }
    patch.push_str(" }\n");
    patch
}

fn demo_project(
    project_id: &str,
) -> (
    WorkspaceSummary,
    WorktreeView,
    HashMap<String, CommitDetails>,
    HashMap<String, TextDiff>,
) {
    let author = "Ada Lovelace <ada@example.com>";
    let other = "Grace Hopper <grace@example.com>";
    let (tip_a, details_a) = commit(DemoCommit {
        seed: "a11a11a",
        title: "Wire the lane header",
        body: "The header shows the branch name and the tip commit.",
        author,
        ago_seconds: 2 * 3600,
        conflicted: false,
        pushed: true,
        files: vec![
            CommitFile {
                path: "src/app.rs".to_string(),
                status: FileStatus::Modified,
                additions: 40,
                deletions: 3,
            },
            CommitFile {
                path: "src/lane.rs".to_string(),
                status: FileStatus::Added,
                additions: 18,
                deletions: 0,
            },
        ],
    });
    let (older_a, details_older_a) = commit(DemoCommit {
        seed: "a00a00a",
        title: "Start the lane",
        body: "First commit on lane-a.",
        author,
        ago_seconds: 26 * 3600,
        conflicted: false,
        pushed: true,
        files: vec![CommitFile {
            path: "src/app.rs".to_string(),
            status: FileStatus::Added,
            additions: 12,
            deletions: 0,
        }],
    });
    let (tip_b, details_b) = commit(DemoCommit {
        seed: "b22b22b",
        title: "Drop the old helper",
        body: "The helper disagreed with the new parser and was removed.",
        author: other,
        ago_seconds: 5 * 3600,
        conflicted: true,
        pushed: false,
        files: vec![CommitFile {
            path: "src/helper.rs".to_string(),
            status: FileStatus::Deleted,
            additions: 0,
            deletions: 22,
        }],
    });
    let (older_b, details_older_b) = commit(DemoCommit {
        seed: "b00b00b",
        title: "Notes for the review",
        body: "Review notes live next to the code they describe.",
        author: other,
        ago_seconds: 30 * 3600,
        conflicted: false,
        pushed: true,
        files: vec![CommitFile {
            path: "docs/notes.md".to_string(),
            status: FileStatus::Added,
            additions: 9,
            deletions: 0,
        }],
    });
    let (tip_c, details_c) = commit(DemoCommit {
        seed: "c33c33c",
        title: "Add the logo asset",
        body: "A binary logo for the about pane.",
        author,
        ago_seconds: 40 * 60,
        conflicted: false,
        pushed: false,
        files: vec![CommitFile {
            path: "assets/logo.bin".to_string(),
            status: FileStatus::Added,
            additions: 0,
            deletions: 0,
        }],
    });
    let (older_c, details_older_c) = commit(DemoCommit {
        seed: "c00c00c",
        title: "Sketch the third lane",
        body: "lane-c holds assets that are not code.",
        author,
        ago_seconds: 3 * 24 * 3600,
        conflicted: false,
        pushed: false,
        files: vec![CommitFile {
            path: "src/new_view.rs".to_string(),
            status: FileStatus::Added,
            additions: 7,
            deletions: 0,
        }],
    });

    let workspace = WorkspaceSummary {
        project_id: project_id.to_string(),
        target_branch: Some("refs/remotes/origin/main".to_string()),
        stacks: vec![
            StackSummary {
                branches: vec![branch("lane-a", vec![tip_a, older_a])],
            },
            StackSummary {
                branches: vec![branch("lane-b", vec![tip_b, older_b])],
            },
            StackSummary {
                branches: vec![branch("lane-c", vec![tip_c, older_c])],
            },
        ],
    };

    let app_hunk_1 = "@@ -1,4 +1,154 @@";
    let app_hunk_2 = "@@ -40,2 +190,152 @@";
    let split_hunk_1 = "@@ -1,2 +1,4 @@";
    let split_hunk_2 = "@@ -8,2 +10,4 @@";
    let files = vec![
        file(
            "src/app.rs",
            FileStatus::Modified,
            None,
            300,
            1,
            vec![
                hunk(app_hunk_1, Some("lane-a")),
                hunk(app_hunk_2, Some("lane-a")),
            ],
        ),
        file(
            "src/split.rs",
            FileStatus::Modified,
            None,
            2,
            0,
            vec![
                hunk(split_hunk_1, Some("lane-a")),
                hunk(split_hunk_2, Some("lane-b")),
            ],
        ),
        file(
            "src/new_view.rs",
            FileStatus::Added,
            None,
            3,
            0,
            vec![hunk("@@ -0,0 +1,3 @@", Some("lane-c"))],
        ),
        file(
            "src/retired.rs",
            FileStatus::Deleted,
            None,
            0,
            3,
            vec![hunk("@@ -1,3 +0,0 @@", None)],
        ),
        file(
            "docs/guide.md",
            FileStatus::Renamed,
            Some("docs/readme.md"),
            1,
            1,
            vec![hunk("@@ -1,1 +1,1 @@", Some("lane-b"))],
        ),
        file(
            "assets/logo.bin",
            FileStatus::Added,
            None,
            0,
            0,
            vec![hunk("", Some("lane-c"))],
        ),
        file(
            "src/util.rs",
            FileStatus::Modified,
            None,
            1,
            0,
            vec![hunk("@@ -1,1 +1,2 @@", None)],
        ),
        file(
            "README.md",
            FileStatus::Modified,
            None,
            1,
            0,
            vec![hunk("@@ -1,1 +1,2 @@", Some("lane-a"))],
        ),
    ];
    let worktree = WorktreeView {
        changed_paths: files.iter().map(|file| file.path.clone()).collect(),
        files,
    };

    let mut commits = HashMap::new();
    for details in [
        details_a,
        details_older_a,
        details_b,
        details_older_b,
        details_c,
        details_older_c,
    ] {
        commits.insert(details.sha.clone(), details);
    }

    let mut diffs = HashMap::new();
    diffs.insert(
        "src/app.rs".to_string(),
        text_diff("src/app.rs", app_patch()),
    );
    diffs.insert(
        "src/split.rs".to_string(),
        text_diff(
            "src/split.rs",
            format!("{split_hunk_1}\n fn left() {{\n+    lane_a();\n }}\n{split_hunk_2}\n fn right() {{\n+    lane_b();\n }}\n"),
        ),
    );
    diffs.insert(
        "src/new_view.rs".to_string(),
        text_diff(
            "src/new_view.rs",
            "@@ -0,0 +1,3 @@\n+fn view() {\n+    render();\n+}\n".to_string(),
        ),
    );
    diffs.insert(
        "src/retired.rs".to_string(),
        text_diff(
            "src/retired.rs",
            "@@ -1,3 +0,0 @@\n-fn retired() {\n-    gone();\n-}\n".to_string(),
        ),
    );
    diffs.insert(
        "docs/guide.md".to_string(),
        text_diff(
            "docs/guide.md",
            "@@ -1,1 +1,1 @@\n-# Readme\n+# Guide\n".to_string(),
        ),
    );
    diffs.insert(
        "assets/logo.bin".to_string(),
        TextDiff {
            path: "assets/logo.bin".to_string(),
            patch: String::new(),
            binary: true,
            too_large: false,
        },
    );
    diffs.insert(
        "src/util.rs".to_string(),
        text_diff(
            "src/util.rs",
            "@@ -1,1 +1,2 @@\n fn util() {}\n+fn extra() {}\n".to_string(),
        ),
    );
    diffs.insert(
        "README.md".to_string(),
        text_diff("README.md", "@@ -1,1 +1,2 @@\n hello\n+more\n".to_string()),
    );

    (workspace, worktree, commits, diffs)
}

impl Backend for FakeBackend {
    fn environment(&self) -> Result<Environment> {
        Ok(self.locked().env.clone())
    }

    fn list_projects(&self) -> Result<ProjectList> {
        self.gate("list_projects")?;
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        Ok(ProjectList {
            projects: state.projects.clone(),
            reason: state.list_reason.clone(),
        })
    }

    fn open_project(&self, id: &str) -> Result<WorkspaceSummary> {
        self.gate("open_project")?;
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        state
            .workspaces
            .get(id)
            .cloned()
            .ok_or_else(|| BridgeError::not_found(format!("No project with id {id}")))
    }

    fn worktree(&self, id: &str) -> Result<WorktreeView> {
        self.gate("worktree")?;
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        state
            .worktrees
            .get(id)
            .cloned()
            .ok_or_else(|| BridgeError::not_found(format!("No worktree for project {id}")))
    }

    fn commit_details(&self, id: &str, sha: &str) -> Result<CommitDetails> {
        self.gate("commit_details")?;
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        if !state.workspaces.contains_key(id) {
            return Err(BridgeError::not_found(format!("No project with id {id}")));
        }
        state
            .commits
            .get(sha)
            .cloned()
            .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))
    }

    fn file_diff(&self, id: &str, request: DiffRequest) -> Result<TextDiff> {
        self.gate("file_diff")?;
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        if !state.workspaces.contains_key(id) {
            return Err(BridgeError::not_found(format!("No project with id {id}")));
        }
        match request {
            DiffRequest::Worktree { path } => {
                state.diffs.get(&path).cloned().ok_or_else(|| {
                    BridgeError::not_found(format!("No uncommitted change for {path}"))
                })
            }
            DiffRequest::Commit { sha, path } => {
                let details = state
                    .commits
                    .get(&sha)
                    .ok_or_else(|| BridgeError::not_found(format!("No commit {sha}")))?;
                if !details.files.iter().any(|file| file.path == path) {
                    return Err(BridgeError::not_found(format!(
                        "{path} is not part of commit {sha}"
                    )));
                }
                if path.ends_with(".bin") {
                    return Ok(TextDiff {
                        path,
                        patch: String::new(),
                        binary: true,
                        too_large: false,
                    });
                }
                Ok(text_diff(
                    &path,
                    format!("@@ -1,1 +1,2 @@\n context\n+change in {path}\n"),
                ))
            }
        }
    }

    fn watch(
        &self,
        id: &str,
        on_change: Box<dyn Fn(BridgeEvent) + Send + Sync>,
    ) -> Result<WatchHandle> {
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        if !state.workspaces.contains_key(id) {
            return Err(BridgeError::not_found(format!("No project with id {id}")));
        }
        drop(state);
        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        self.watchers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(id.to_string())
            .or_default()
            .push(FakeWatch {
                token,
                callback: Arc::from(on_change),
            });
        Ok(WatchHandle::fake(
            id.to_string(),
            token,
            Arc::clone(&self.watchers),
            true,
        ))
    }

    fn is_preview(&self) -> bool {
        true
    }

    fn preview_latency_ms(&self) -> u64 {
        self.latency_ms.load(Ordering::Relaxed)
    }

    fn set_preview_latency_ms(&self, ms: u64) {
        self.latency_ms.store(ms, Ordering::Relaxed);
    }

    fn arm_preview_fail(&self, op: &str) {
        self.fail_ops
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(op.to_string());
    }

    fn trigger_fake(&self, id: &str, kind: &str) -> Result<()> {
        if kind == "askpass" {
            let request = {
                let mut state = self.locked();
                ensure_project(&state, id)?;
                let request = AskpassRequest {
                    id: format!("ask-{}", self.tokens.fetch_add(1, Ordering::Relaxed)),
                    prompt: "Password for 'https://example.test': ".to_string(),
                    kind: AskpassKind::Credential,
                };
                state.askpass.push(request.clone());
                request
            };
            self.notify_askpass(request);
            return Ok(());
        }
        let event = match kind {
            "worktree" => {
                let state = self.locked();
                let view = state.worktrees.get(id).cloned().ok_or_else(|| {
                    BridgeError::not_found(format!("No worktree for project {id}"))
                })?;
                BridgeEvent::Worktree(view)
            }
            "git" => BridgeEvent::GitActivity {
                head_sha: sha("a11a11a").0,
            },
            other => {
                return Err(BridgeError::not_found(format!(
                    "trigger-event kind must be worktree, git, or askpass, not {other}"
                )));
            }
        };
        self.trigger(id, event);
        Ok(())
    }

    fn assign(
        &self,
        id: &str,
        selections: &[SelectionSpec],
        lane: Option<&str>,
    ) -> Result<WorkspaceSummary> {
        self.gate("assign")?;
        let workspace = assign_in(&mut self.locked(), id, selections, lane)?;
        self.publish(id);
        Ok(workspace)
    }

    fn discard(&self, id: &str, selections: &[SelectionSpec]) -> Result<DiscardResult> {
        self.gate("discard")?;
        let result = discard_in(&mut self.locked(), id, selections)?;
        self.publish(id);
        Ok(result)
    }

    fn create_lane(&self, id: &str, name: &str) -> Result<LaneCreated> {
        self.gate("create_lane")?;
        let created = create_lane_in(&mut self.locked(), id, name)?;
        self.publish(id);
        Ok(created)
    }

    fn commit_create(
        &self,
        id: &str,
        anchor: &Anchor,
        selections: &[SelectionSpec],
        message: &str,
    ) -> Result<CommitResult> {
        self.gate("commit_create")?;
        let result = commit_create_in(&mut self.locked(), id, anchor, selections, message)?;
        self.publish(id);
        Ok(result)
    }

    fn amend(&self, id: &str, sha: &str, selections: &[SelectionSpec]) -> Result<CommitResult> {
        self.gate("amend")?;
        let result = amend_in(&mut self.locked(), id, sha, selections)?;
        self.publish(id);
        Ok(result)
    }

    fn reword(&self, id: &str, sha: &str, message: &str) -> Result<CommitResult> {
        self.gate("reword")?;
        let result = reword_in(&mut self.locked(), id, sha, message)?;
        self.publish(id);
        Ok(result)
    }

    fn squash(&self, id: &str, subject_shas: &[String], target_sha: &str) -> Result<CommitResult> {
        self.gate("squash")?;
        let result = squash_in(&mut self.locked(), id, subject_shas, target_sha)?;
        self.publish(id);
        Ok(result)
    }

    fn uncommit(&self, id: &str, shas: &[String], lane: Option<&str>) -> Result<CommitResult> {
        self.gate("uncommit")?;
        let result = uncommit_in(&mut self.locked(), id, shas, lane)?;
        self.publish(id);
        Ok(result)
    }

    fn uncommit_changes(
        &self,
        id: &str,
        sha: &str,
        selections: &[SelectionSpec],
        lane: Option<&str>,
    ) -> Result<CommitResult> {
        self.gate("uncommit_changes")?;
        let result = uncommit_changes_in(&mut self.locked(), id, sha, selections, lane)?;
        self.publish(id);
        Ok(result)
    }

    fn move_commit(&self, id: &str, shas: &[String], anchor: &Anchor) -> Result<CommitResult> {
        self.gate("move_commit")?;
        let result = move_commit_in(&mut self.locked(), id, shas, anchor)?;
        self.publish(id);
        Ok(result)
    }

    fn insert_blank(&self, id: &str, anchor: &Anchor) -> Result<CommitResult> {
        self.gate("insert_blank")?;
        let result = insert_blank_in(&mut self.locked(), id, anchor)?;
        self.publish(id);
        Ok(result)
    }

    fn move_changes(
        &self,
        id: &str,
        source_sha: &str,
        destination_sha: &str,
        selections: &[SelectionSpec],
    ) -> Result<CommitResult> {
        self.gate("move_changes")?;
        let result = move_changes_in(
            &mut self.locked(),
            id,
            source_sha,
            destination_sha,
            selections,
        )?;
        self.publish(id);
        Ok(result)
    }

    fn snapshots(&self, id: &str, limit: usize) -> Result<Vec<SnapshotInfo>> {
        self.gate("snapshots")?;
        snapshots_in(&self.locked(), id, limit)
    }

    fn restore_snapshot(&self, id: &str, sha: &str) -> Result<UndoResult> {
        self.gate("restore_snapshot")?;
        let result = restore_in(&mut self.locked(), id, sha)?;
        self.publish(id);
        Ok(result)
    }

    fn undo_last(&self, id: &str) -> Result<UndoResult> {
        self.gate("undo_last")?;
        let result = undo_in(&mut self.locked(), id)?;
        self.publish(id);
        Ok(result)
    }

    fn snapshot_diff(&self, id: &str, sha: &str) -> Result<Vec<SnapshotChange>> {
        self.gate("snapshot_diff")?;
        snapshot_diff_in(&self.locked(), id, sha)
    }

    fn create_snapshot(&self, id: &str, message: &str) -> Result<SnapshotInfo> {
        self.gate("create_snapshot")?;
        let snapshot = create_snapshot_in(&mut self.locked(), id, message)?;
        self.publish(id);
        Ok(snapshot)
    }

    fn list_branches(&self, id: &str) -> Result<Vec<BranchRow>> {
        self.gate("list_branches")?;
        list_branches_in(&self.locked(), id)
    }

    fn branch_details(&self, id: &str, ref_name: &str) -> Result<BranchDetails> {
        self.gate("branch_details")?;
        branch_details_in(&self.locked(), id, ref_name)
    }

    fn apply_branch(&self, id: &str, ref_name: &str) -> Result<ApplyResult> {
        self.gate("apply_branch")?;
        let result = apply_branch_in(&mut self.locked(), id, ref_name)?;
        self.publish(id);
        Ok(result)
    }

    fn unapply_branch(&self, id: &str, name: &str) -> Result<WorkspaceSummary> {
        self.gate("unapply_branch")?;
        let workspace = unapply_branch_in(&mut self.locked(), id, name)?;
        self.publish(id);
        Ok(workspace)
    }

    fn delete_local_branch(&self, id: &str, name: &str) -> Result<WorkspaceSummary> {
        self.gate("delete_local_branch")?;
        let workspace = delete_local_branch_in(&mut self.locked(), id, name)?;
        self.publish(id);
        Ok(workspace)
    }

    fn edit_status(&self, id: &str) -> Result<EditStatus> {
        self.gate("edit_status")?;
        edit_status_in(&self.locked(), id)
    }

    fn enter_edit_mode(&self, id: &str, sha: &str) -> Result<EditStatus> {
        self.gate("enter_edit_mode")?;
        let status = enter_edit_in(&mut self.locked(), id, sha)?;
        self.publish_head(id, true);
        self.publish(id);
        Ok(status)
    }

    fn save_edit_and_return_to_workspace(&self, id: &str) -> Result<WorkspaceSummary> {
        self.gate("save_edit_and_return_to_workspace")?;
        let workspace = save_edit_in(&mut self.locked(), id)?;
        self.publish_head(id, false);
        self.publish(id);
        Ok(workspace)
    }

    fn abort_edit_and_return_to_workspace(
        &self,
        id: &str,
        force: bool,
    ) -> Result<WorkspaceSummary> {
        self.gate("abort_edit_and_return_to_workspace")?;
        let workspace = abort_edit_in(&mut self.locked(), id, force)?;
        self.publish_head(id, false);
        self.publish(id);
        Ok(workspace)
    }

    fn conflicted_commits(&self, id: &str) -> Result<Vec<ConflictedCommit>> {
        self.gate("conflicted_commits")?;
        conflicted_commits_in(&self.locked(), id)
    }

    fn resolve_commit_conflict_hunks(
        &self,
        id: &str,
        sha: &str,
        files: &[FileChoice],
    ) -> Result<ResolveReport> {
        self.gate("resolve_commit_conflict_hunks")?;
        let report = resolve_in(&mut self.locked(), id, sha, files)?;
        self.publish(id);
        Ok(report)
    }

    fn pre_commit_hook(&self, id: &str, selections: &[SelectionSpec]) -> Result<HookReport> {
        require_selection(selections)?;
        hook_in(&self.locked(), id)
    }

    fn post_commit_hook(&self, id: &str) -> Result<HookReport> {
        hook_in(&self.locked(), id)
    }

    fn message_hook(&self, id: &str, message: &str) -> Result<HookReport> {
        require_message(message)?;
        hook_in(&self.locked(), id)
    }

    fn add_project(&self, path: &str) -> Result<ProjectSummary> {
        self.gate("add_project")?;
        let mut state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        if path.trim().is_empty() {
            return Err(BridgeError::not_ready("Choose a repository folder."));
        }
        if let Some(existing) = state.projects.iter().find(|project| project.path == path) {
            return Ok(existing.clone());
        }
        state.next_sha += 1;
        let id = format!("project-{}", state.next_sha);
        let title = path
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("project")
            .to_string();
        let summary = ProjectSummary {
            id: id.clone(),
            title,
            path: path.to_string(),
        };
        state.projects.push(summary.clone());
        state.workspaces.insert(
            id.clone(),
            WorkspaceSummary {
                project_id: id.clone(),
                target_branch: None,
                stacks: Vec::new(),
            },
        );
        state.worktrees.insert(
            id.clone(),
            WorktreeView {
                files: Vec::new(),
                changed_paths: Vec::new(),
            },
        );
        state.remotes.insert(id, FakeRemote::demo());
        Ok(summary)
    }

    fn delete_project(&self, id: &str) -> Result<()> {
        self.gate("delete_project")?;
        let mut state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        let Some(index) = state.projects.iter().position(|project| project.id == id) else {
            return Err(BridgeError::not_found(format!("No project with id {id}")));
        };
        state.projects.remove(index);
        state.workspaces.remove(id);
        state.worktrees.remove(id);
        state.remotes.remove(id);
        state.catalog.remove(id);
        state.edits.remove(id);
        Ok(())
    }

    fn base_branch(&self, id: &str) -> Result<Option<BaseBranchData>> {
        fake_base(&self.locked(), id)
    }

    fn remote_branches(&self, id: &str) -> Result<Vec<RemoteBranch>> {
        self.gate("remote_branches")?;
        ensure_project(&self.locked(), id)?;
        Ok(vec![
            RemoteBranch {
                remote: "origin".to_string(),
                branch: "main".to_string(),
                full_name: "refs/remotes/origin/main".to_string(),
            },
            RemoteBranch {
                remote: "origin".to_string(),
                branch: "topic".to_string(),
                full_name: "refs/remotes/origin/topic".to_string(),
            },
        ])
    }

    fn set_target(
        &self,
        id: &str,
        target_ref: &str,
        push_remote: Option<&str>,
    ) -> Result<BaseBranchData> {
        self.gate("set_target")?;
        let base = {
            let mut state = self.locked();
            ensure_project(&state, id)?;
            refuse_edit(&state, id)?;
            if target_ref.trim().is_empty() {
                return Err(BridgeError::not_ready("Choose a target branch."));
            }
            if let Some(workspace) = state.workspaces.get_mut(id) {
                workspace.target_branch = Some(target_ref.to_string());
            }
            if let Some(remote_name) = push_remote.map(str::trim).filter(|name| !name.is_empty()) {
                state
                    .remotes
                    .entry(id.to_string())
                    .or_insert_with(FakeRemote::demo)
                    .push_remote = remote_name.to_string();
            }
            fake_base(&state, id)?
        };
        self.publish(id);
        base.ok_or_else(|| BridgeError::not_ready("That project has no target branch."))
    }

    fn fetch(&self, id: &str) -> Result<FetchReport> {
        self.gate("fetch")?;
        let report = {
            let mut state = self.locked();
            ensure_project(&state, id)?;
            refuse_edit(&state, id)?;
            {
                let remote = state
                    .remotes
                    .entry(id.to_string())
                    .or_insert_with(FakeRemote::demo);
                if remote.fetched.is_empty() {
                    let waiting = std::mem::take(&mut remote.waiting);
                    remote.fetched.extend(waiting);
                }
                let now = now_ms();
                remote.last_attempted_ms = Some(now);
                remote.last_fetched_ms = Some(now);
                remote.last_error = None;
            }
            let status = fake_status(&state, id)?;
            let summary = status.summary.clone();
            FetchReport {
                status,
                steps: vec!["Fetched remotes".to_string(), summary],
            }
        };
        self.trigger(id, BridgeEvent::GitFetch);
        self.publish(id);
        Ok(report)
    }

    fn fetch_status(&self, id: &str) -> Result<FetchStatus> {
        fake_status(&self.locked(), id)
    }

    fn integrate_preview(&self, id: &str, strategy: UpstreamStrategy) -> Result<IntegratePreview> {
        self.gate("integrate_preview")?;
        let state = self.locked();
        ensure_project(&state, id)?;
        refuse_edit(&state, id)?;
        let resolutions = lane_resolutions(&state, id, strategy);
        let count = resolutions.len();
        let lanes = if count == 1 { "lane" } else { "lanes" };
        let summary = match strategy {
            UpstreamStrategy::Rebase => format!("{count} {lanes} will rebase onto the upstream."),
            UpstreamStrategy::Merge => format!("{count} {lanes} will merge the upstream."),
        };
        Ok(IntegratePreview {
            resolutions,
            worktree_conflicts: Vec::new(),
            summary,
        })
    }

    fn integrate(&self, id: &str, strategy: UpstreamStrategy) -> Result<IntegrateReport> {
        self.gate("integrate")?;
        let report = {
            let mut state = self.locked();
            ensure_project(&state, id)?;
            refuse_edit(&state, id)?;
            if let Some(remote) = state.remotes.get_mut(id) {
                remote.fetched.clear();
                remote.waiting.clear();
            }
            let resolutions = lane_resolutions(&state, id, strategy);
            let workspace = state
                .workspaces
                .get(id)
                .cloned()
                .unwrap_or_else(|| WorkspaceSummary {
                    project_id: id.to_string(),
                    target_branch: None,
                    stacks: Vec::new(),
                });
            IntegrateReport {
                workspace,
                resolutions,
                worktree_conflicts: Vec::new(),
            }
        };
        self.publish(id);
        Ok(report)
    }

    fn push(&self, id: &str, lane: &str, force: ForcePush) -> Result<PushReport> {
        self.gate("push")?;
        let report = {
            let mut state = self.locked();
            ensure_project(&state, id)?;
            refuse_edit(&state, id)?;
            let workspace = state
                .workspaces
                .get_mut(id)
                .ok_or_else(|| BridgeError::not_found(format!("No project with id {id}")))?;
            let branch = workspace
                .stacks
                .iter_mut()
                .find_map(|stack| stack.branches.iter_mut().find(|branch| branch.name == lane))
                .ok_or_else(|| BridgeError::not_found(format!("No lane named {lane}")))?;
            let already = branch.commits.iter().all(|commit| commit.pushed);
            let after_sha = branch
                .commits
                .first()
                .map(|commit| commit.sha.clone())
                .unwrap_or_default();
            if already {
                PushReport {
                    remote: "origin".to_string(),
                    branches: Vec::new(),
                    summary: "Nothing to push".to_string(),
                    force,
                    steps: vec!["Nothing to push".to_string()],
                }
            } else {
                for commit in &mut branch.commits {
                    commit.pushed = true;
                }
                let summary = format!("Pushed {lane}");
                PushReport {
                    remote: "origin".to_string(),
                    branches: vec![PushedBranch {
                        name: lane.to_string(),
                        remote_ref: format!("refs/remotes/origin/{lane}"),
                        remote_branch: lane.to_string(),
                        before_sha: String::new(),
                        after_sha,
                    }],
                    summary: summary.clone(),
                    force,
                    steps: vec![summary],
                }
            }
        };
        self.trigger(id, BridgeEvent::GitFetch);
        self.publish(id);
        Ok(report)
    }

    fn askpass_pending(&self) -> Result<Vec<AskpassRequest>> {
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        Ok(state.askpass.clone())
    }

    fn watch_askpass(
        &self,
        on_prompt: Box<dyn Fn(AskpassRequest) + Send + Sync>,
    ) -> Result<AskpassWatch> {
        {
            let state = self.locked();
            if state.env.compat != Compat::Match {
                return Err(crate::compat_error(&state.env));
            }
        }
        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        self.askpass_watchers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push((token, Arc::from(on_prompt)));
        let watchers = Arc::clone(&self.askpass_watchers);
        Ok(AskpassWatch::new(move || {
            watchers
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .retain(|(have, _)| *have != token);
        }))
    }

    fn answer_askpass(&self, id: &str, response: Option<String>) -> Result<()> {
        let _secret = crate::Secret::new(response);
        let mut state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        let Some(index) = state.askpass.iter().position(|request| request.id == id) else {
            return Err(BridgeError::not_found("That prompt is no longer waiting."));
        };
        state.askpass.remove(index);
        Ok(())
    }

    fn forge_status(&self, id: &str) -> Result<ForgeStatus> {
        reviews_of(&self.locked(), id)
    }

    fn review_for_branch(&self, id: &str, branch: &str) -> Result<Option<Review>> {
        review_of(&self.locked(), id, branch)
    }

    fn publish_draft(&self, id: &str, branch: &str) -> Result<Review> {
        self.gate("publish_draft")?;
        publish_of(&mut self.locked(), id, branch)
    }

    fn suggest_message(&self, id: &str, paths: &[String]) -> Result<SuggestedMessage> {
        self.gate("suggest_message")?;
        ensure_project(&self.locked(), id)?;
        FakeMessageGenerator.suggest(paths)
    }
}

impl Settings for FakeBackend {
    fn read_global(&self) -> Result<GlobalSettings> {
        self.gate("read_global")?;
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        Ok(state.global.clone())
    }

    fn update_global(&self, patch: &GlobalPatch) -> Result<GlobalSettings> {
        self.gate("update_global")?;
        let mut state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        state.global.apply(patch);
        Ok(state.global.clone())
    }

    fn read_project(&self, id: &str) -> Result<ProjectSettings> {
        self.gate("read_project")?;
        let state = self.locked();
        if state.env.compat != Compat::Match {
            return Err(crate::compat_error(&state.env));
        }
        if !state.projects.iter().any(|project| project.id == id) {
            return Err(BridgeError::not_found(format!("No project with id {id}")));
        }
        let target = state
            .workspaces
            .get(id)
            .and_then(|workspace| workspace.target_branch.clone());
        Ok(ProjectSettings {
            target_branch: target,
            signing: SigningReport::off(),
            remove_allowed: true,
        })
    }

    fn forget_project(&self, id: &str) -> Result<()> {
        self.gate("forget_project")?;
        self.delete_project(id)
    }
}

impl Forge for FakeBackend {
    fn reviews(&self, id: &str) -> Result<ForgeStatus> {
        reviews_of(&self.locked(), id)
    }

    fn review_for_branch(&self, id: &str, branch: &str) -> Result<Option<Review>> {
        review_of(&self.locked(), id, branch)
    }

    fn publish_draft(&self, id: &str, branch: &str) -> Result<Review> {
        self.gate("publish_draft")?;
        publish_of(&mut self.locked(), id, branch)
    }
}

fn latency_from_env() -> u64 {
    std::env::var("GB_GPUI_FAKE_LATENCY_MS")
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn forced_fail(op: &str) -> String {
    match op {
        "fetch" => "Could not reach the remote. You might be offline. Fetch again when the network is back.".to_string(),
        "push" => "The push did not finish. The remote may be offline. Try again.".to_string(),
        "integrate" | "integrate_preview" => {
            "Could not integrate with the remote. You might be offline. Try again.".to_string()
        }
        other => format!("Could not finish {other}. Try again."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::MessageGenerator;
    use crate::{Backend, Forge};
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_fake_forge_remembers_a_draft_and_the_message_follows_file_names() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        let status = backend.forge_status(id).unwrap();
        assert!(status.connected);
        assert!(
            status.reviews.iter().any(|review| {
                review.branch == "lane-a" && review.number == 128 && !review.draft
            })
        );
        assert!(
            Backend::review_for_branch(&backend, id, "lane-c")
                .unwrap()
                .is_none()
        );
        let created = Backend::publish_draft(&backend, id, "lane-c").unwrap();
        assert_eq!(created.number, 132);
        assert!(created.draft);
        let again = Backend::publish_draft(&backend, id, "refs/heads/lane-c").unwrap();
        assert_eq!(again.number, created.number);
        let listed = Forge::reviews(&backend, id).unwrap();
        assert_eq!(
            listed
                .reviews
                .iter()
                .filter(|review| review.branch == "lane-c")
                .count(),
            1
        );
        let remote = Backend::publish_draft(&backend, id, "remote-only").unwrap_err();
        assert!(
            remote.message.contains("local branch"),
            "{}",
            remote.message
        );
        let message = backend
            .suggest_message(id, &["src/app.rs".to_string(), "README.md".to_string()])
            .unwrap();
        assert_eq!(
            message,
            FakeMessageGenerator
                .suggest(&["README.md".to_string(), "src/app.rs".to_string()])
                .unwrap()
        );
    }

    fn lane_commits(workspace: &WorkspaceSummary, name: &str) -> Vec<String> {
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

    #[test]
    fn committing_one_hunk_leaves_the_other_and_undo_puts_it_back() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        let before = backend.worktree(id).unwrap();
        let split = before
            .files
            .iter()
            .find(|file| file.path == "src/split.rs")
            .unwrap();
        assert_eq!(split.hunks.len(), 2);
        let hunk = HunkRef {
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 4,
        };
        let created = backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::hunks("src/split.rs", vec![hunk])],
                "commit the left hunk",
            )
            .unwrap();
        assert!(created.rejected.is_empty());
        let sha = created.new_sha.expect("sha");
        assert_eq!(
            lane_commits(&created.workspace, "lane-a")[0],
            "commit the left hunk"
        );
        let details = backend.commit_details(id, &sha).unwrap();
        assert_eq!(details.files.len(), 1);
        assert_eq!(details.files[0].path, "src/split.rs");
        let after = backend.worktree(id).unwrap();
        let split = after
            .files
            .iter()
            .find(|file| file.path == "src/split.rs")
            .expect("the other hunk stays");
        assert_eq!(split.hunks.len(), 1);
        assert_eq!(split.hunks[0].header, "@@ -8,2 +10,4 @@");

        let undone = backend.undo_last(id).unwrap();
        assert!(!undone.restored_sha.is_empty());
        let restored = backend.worktree(id).unwrap();
        let split = restored
            .files
            .iter()
            .find(|file| file.path == "src/split.rs")
            .unwrap();
        assert_eq!(split.hunks.len(), 2);
        assert!(
            !lane_commits(&undone.workspace, "lane-a")
                .iter()
                .any(|title| title == "commit the left hunk")
        );

        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::whole("src/util.rs")],
                "after undo",
            )
            .unwrap();
        let snapshots = backend.snapshots(id, 10).unwrap();
        assert!(
            snapshots
                .iter()
                .any(|snapshot| snapshot.title == "Created commit"
                    && snapshot.operation == "CreateCommit")
        );
        backend.undo_last(id).unwrap();
        assert!(
            !lane_commits(&backend.open_project(id).unwrap(), "lane-a")
                .iter()
                .any(|title| title == "after undo")
        );
    }

    #[test]
    fn assign_moves_between_lanes_and_unassign_clears_them() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        backend
            .assign(id, &[SelectionSpec::whole("README.md")], Some("lane-b"))
            .unwrap();
        let file = backend
            .worktree(id)
            .unwrap()
            .files
            .into_iter()
            .find(|file| file.path == "README.md")
            .unwrap();
        assert_eq!(file.lanes(), ["lane-b"]);
        backend
            .assign(id, &[SelectionSpec::whole("README.md")], None)
            .unwrap();
        let file = backend
            .worktree(id)
            .unwrap()
            .files
            .into_iter()
            .find(|file| file.path == "README.md")
            .unwrap();
        assert!(file.lanes().is_empty(), "{file:?}");
    }

    #[test]
    fn discard_amend_reword_squash_uncommit_and_move() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        backend
            .discard(id, &[SelectionSpec::whole("README.md")])
            .unwrap();
        assert!(
            backend
                .worktree(id)
                .unwrap()
                .files
                .iter()
                .all(|file| file.path != "README.md")
        );
        let amended = backend.amend(
            id,
            "util is not a commit",
            &[SelectionSpec::whole("src/util.rs")],
        );
        assert!(amended.is_err());
        let tip = backend.open_project(id).unwrap().stacks[0].branches[0].commits[0]
            .sha
            .clone();
        let amended = backend
            .amend(id, &tip, &[SelectionSpec::whole("src/util.rs")])
            .unwrap();
        let new_tip = amended.new_sha.expect("amended");
        let details = backend.commit_details(id, &new_tip).unwrap();
        assert!(details.files.iter().any(|file| file.path == "src/util.rs"));

        let reworded = backend.reword(id, &new_tip, "renamed tip").unwrap();
        assert_eq!(
            lane_commits(&reworded.workspace, "lane-a")[0],
            "renamed tip"
        );
        let squashed_tip = reworded.new_sha.unwrap();
        let older = reworded.workspace.stacks[0].branches[0].commits[1]
            .sha
            .clone();
        let squashed = backend.squash(id, &[squashed_tip], &older).unwrap();
        let message = backend
            .commit_details(id, squashed.new_sha.as_deref().unwrap())
            .unwrap()
            .message;
        assert!(message.contains("renamed tip"), "{message}");
        assert!(message.contains('\n'), "{message}");

        let blank = backend
            .insert_blank(id, &Anchor::Lane("lane-b".to_string()))
            .unwrap();
        assert_eq!(
            lane_commits(&blank.workspace, "lane-b")[0],
            "(empty commit)"
        );
        let blank_sha = blank.new_sha.unwrap();
        let moved = backend
            .move_commit(id, &[blank_sha], &Anchor::Lane("lane-c".to_string()))
            .unwrap();
        assert_eq!(
            lane_commits(&moved.workspace, "lane-c")[0],
            "(empty commit)"
        );
        assert_ne!(
            lane_commits(&moved.workspace, "lane-b")[0],
            "(empty commit)"
        );

        let created = backend.create_lane(id, "lane-d").unwrap();
        assert!(
            created
                .workspace
                .stacks
                .iter()
                .any(|stack| { stack.names().any(|name| name == "lane-d") })
        );
        let hook = backend
            .pre_commit_hook(id, &[SelectionSpec::whole("src/util.rs")])
            .unwrap();
        assert_eq!(hook.status, HookStatus::NotConfigured);
        assert!(backend.message_hook(id, "   ").is_err());
        let mismatch = FakeBackend::mismatched("9.9.9");
        assert!(mismatch.create_lane("demo-project", "lane-z").is_err());
    }

    #[test]
    fn watchers_hear_a_commit() {
        let backend = FakeBackend::demo();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::clone(&seen);
        let _watch = backend
            .watch(
                "demo-project",
                Box::new(move |event| {
                    slot.lock().unwrap().push(event.name().to_string());
                }),
            )
            .unwrap();
        backend
            .commit_create(
                "demo-project",
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::whole("src/util.rs")],
                "notify",
            )
            .unwrap();
        let names = seen.lock().unwrap().clone();
        assert!(
            names.iter().any(|name| name == "worktreeChanges"),
            "{names:?}"
        );
        assert!(
            names.iter().any(|name| name == "workspaceActivity"),
            "{names:?}"
        );
    }

    #[test]
    fn fetch_integrate_push_and_a_scripted_prompt() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        let before = backend.fetch_status(id).unwrap();
        assert_eq!(before.upstream_count, 0);
        assert_eq!(before.summary, "Up to date");
        assert!(before.last_fetched_ms.is_none());

        let branches = backend.remote_branches(id).unwrap();
        assert!(
            branches
                .iter()
                .any(|branch| branch.full_name == "refs/remotes/origin/main")
        );
        assert!(branches.iter().any(|branch| branch.branch == "topic"));

        let target = backend
            .set_target(id, "refs/remotes/origin/main", Some("origin"))
            .unwrap();
        assert_eq!(target.short_name, "main");
        assert_eq!(target.push_remote_name, "origin");

        let fetched = backend.fetch(id).unwrap();
        assert_eq!(fetched.status.upstream_count, 2);
        assert_eq!(fetched.status.summary, "2 new upstream commits");
        assert_eq!(
            fetched
                .status
                .commits
                .iter()
                .map(|commit| commit.title.as_str())
                .collect::<Vec<_>>(),
            ["Upstream one", "Upstream two"]
        );
        assert!(fetched.status.last_fetched_ms.is_some());

        let preview = backend
            .integrate_preview(id, UpstreamStrategy::Rebase)
            .unwrap();
        assert!(
            preview
                .resolutions
                .iter()
                .any(|lane| lane.lane == "lane-c" && lane.resolution == IntegrateResolution::Clear)
        );
        assert_eq!(backend.fetch_status(id).unwrap().upstream_count, 2);

        let applied = backend.integrate(id, UpstreamStrategy::Rebase).unwrap();
        assert!(
            applied
                .workspace
                .stacks
                .iter()
                .any(|stack| stack.names().any(|name| name == "lane-c"))
        );
        assert_eq!(backend.fetch_status(id).unwrap().upstream_count, 0);

        let unpushed = backend.open_project(id).unwrap();
        let lane_c = unpushed
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-c")
            .unwrap();
        assert!(lane_c.commits.iter().all(|commit| !commit.pushed));
        let pushed = backend.push(id, "lane-c", ForcePush::No).unwrap();
        assert_eq!(pushed.summary, "Pushed lane-c");
        let after = backend.open_project(id).unwrap();
        let lane_c = after
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-c")
            .unwrap();
        assert!(lane_c.commits.iter().all(|commit| commit.pushed));
        assert_eq!(
            backend.push(id, "lane-c", ForcePush::No).unwrap().summary,
            "Nothing to push"
        );

        backend.trigger_fake(id, "askpass").unwrap();
        let pending = backend.askpass_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, AskpassKind::Credential);
        assert!(!format!("{:?}", pending[0]).contains("s3cret"));
        let mut prompts = crate::FakeAskpass::scripted([Some("s3cret".to_string())]);
        assert_eq!(prompts.respond(&backend).unwrap(), 1);
        assert!(backend.askpass_pending().unwrap().is_empty());

        backend.trigger_fake(id, "askpass").unwrap();
        backend
            .answer_askpass(&backend.askpass_pending().unwrap()[0].id, None)
            .unwrap();
        assert!(backend.askpass_pending().unwrap().is_empty());

        let added = backend.add_project("/tmp/other-repo").unwrap();
        assert_eq!(backend.add_project("/tmp/other-repo").unwrap().id, added.id);
        backend.delete_project(&added.id).unwrap();
        assert!(
            backend
                .list_projects()
                .unwrap()
                .projects
                .iter()
                .all(|project| project.id != added.id)
        );
        assert!(backend.fetch("missing").is_err());
        assert!(FakeBackend::mismatched("9.9.9").fetch(id).is_err());
    }

    fn row<'a>(rows: &'a [BranchRow], name: &str) -> &'a BranchRow {
        rows.iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("no branch {name} in {rows:?}"))
    }

    #[test]
    fn branches_apply_unapply_and_delete() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        let listed = backend.list_branches(id).unwrap();
        assert!(row(&listed, "lane-a").applied);
        assert!(row(&listed, "lane-b").applied);
        assert!(row(&listed, "lane-c").applied);
        let topic = row(&listed, "topic");
        assert!(!topic.applied);
        assert_eq!(topic.place, BranchPlace::Both);
        assert_eq!(topic.ahead, Some(1));
        assert_eq!(topic.behind, Some(0));
        assert_eq!(topic.last_commit, "Start the topic");
        let remote = row(&listed, "remote-only");
        assert!(!remote.applied);
        assert_eq!(remote.place, BranchPlace::Remote);

        let details = backend.branch_details(id, "topic").unwrap();
        assert_eq!(details.commits[0].title, "Start the topic");
        assert!(!details.applied);

        let applied = backend.apply_branch(id, "refs/heads/topic").unwrap();
        assert!(!applied.already_applied);
        assert_eq!(applied.names, ["topic"]);
        assert_eq!(applied.workspace.stacks.len(), 4);
        assert!(row(&backend.list_branches(id).unwrap(), "topic").applied);

        let refused = backend.delete_local_branch(id, "topic").unwrap_err();
        assert!(refused.message.contains("Unapply"), "{}", refused.message);
        assert_eq!(backend.open_project(id).unwrap().stacks.len(), 4);

        let unapplied = backend.unapply_branch(id, "topic").unwrap();
        assert_eq!(unapplied.stacks.len(), 3);
        assert!(!row(&backend.list_branches(id).unwrap(), "topic").applied);
        backend.delete_local_branch(id, "refs/heads/topic").unwrap();
        assert!(
            backend
                .list_branches(id)
                .unwrap()
                .iter()
                .all(|row| row.name != "topic")
        );

        let remote_delete = backend.delete_local_branch(id, "remote-only").unwrap_err();
        assert!(
            remote_delete.message.contains("local"),
            "{}",
            remote_delete.message
        );
        let remote_apply = backend
            .apply_branch(id, "refs/remotes/origin/remote-only")
            .unwrap_err();
        assert!(
            remote_apply.message.contains("remote"),
            "{}",
            remote_apply.message
        );
    }

    #[test]
    fn snapshot_restore_is_undoable() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        let checkpoint = backend.create_snapshot(id, "checkpoint").unwrap();
        assert_eq!(checkpoint.operation, "OnDemandSnapshot");
        assert_eq!(checkpoint.title, "checkpoint");
        assert!(checkpoint.restorable);

        backend
            .discard(id, &[SelectionSpec::whole("README.md")])
            .unwrap();
        assert!(
            backend
                .worktree(id)
                .unwrap()
                .files
                .iter()
                .all(|file| file.path != "README.md")
        );
        let listed = backend.list_snapshots(id, 20).unwrap();
        assert!(
            listed.iter().any(|snapshot| {
                snapshot.sha == checkpoint.sha
                    && snapshot.operation == "OnDemandSnapshot"
                    && snapshot.title == "checkpoint"
                    && snapshot.restorable
            }),
            "{listed:?}"
        );
        let diff = backend.snapshot_diff(id, &checkpoint.sha).unwrap();
        assert!(
            diff.iter()
                .any(|change| change.path == "README.md" && change.status == FileStatus::Deleted),
            "{diff:?}"
        );

        backend.restore_snapshot(id, &checkpoint.sha).unwrap();
        assert!(
            backend
                .worktree(id)
                .unwrap()
                .files
                .iter()
                .any(|file| file.path == "README.md")
        );
        assert!(
            backend
                .list_snapshots(id, 20)
                .unwrap()
                .iter()
                .any(|snapshot| snapshot.operation == "RestoreFromSnapshot" && snapshot.restorable)
        );

        backend.undo_last(id).unwrap();
        assert!(
            backend
                .worktree(id)
                .unwrap()
                .files
                .iter()
                .all(|file| file.path != "README.md")
        );
    }

    fn conflicted_sha(backend: &FakeBackend) -> String {
        let commits = backend.conflicted_commits("demo-project").unwrap();
        assert_eq!(commits.len(), 1, "{commits:?}");
        assert_eq!(commits[0].title, "Drop the old helper");
        assert_eq!(commits[0].lane, "lane-b");
        commits[0].sha.clone()
    }

    #[test]
    fn edit_mode_save_rewrites_the_commit_and_blocks_other_writes() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        let sha = conflicted_sha(&backend);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::clone(&seen);
        let _watch = backend
            .watch(
                id,
                Box::new(move |event| {
                    slot.lock().unwrap().push(event.name().to_string());
                }),
            )
            .unwrap();

        let status = backend.enter_edit_mode(id, &sha).unwrap();
        assert!(status.editing);
        assert_eq!(status.commit_sha.as_deref(), Some(sha.as_str()));
        let blocked = backend.create_lane(id, "lane-z").unwrap_err();
        assert!(blocked.message.contains("edit mode"), "{}", blocked.message);
        let blocked_snapshot = backend.create_snapshot(id, "nope").unwrap_err();
        assert!(
            blocked_snapshot.message.contains("edit mode"),
            "{}",
            blocked_snapshot.message
        );

        backend.edit_file(id, "src/helper.rs", "kept\n").unwrap();
        let saved = backend.save_edit_and_return_to_workspace(id).unwrap();
        let tip = saved
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-b")
            .unwrap()
            .commits[0]
            .clone();
        assert_ne!(tip.sha, sha);
        assert_eq!(tip.title, "Drop the old helper");
        let details = backend.commit_details(id, &tip.sha).unwrap();
        assert!(
            details.message.contains("Edited src/helper.rs."),
            "{details:?}"
        );
        assert!(details.message.contains("kept"), "{details:?}");
        assert!(!backend.edit_status(id).unwrap().editing);
        let names = seen.lock().unwrap().clone();
        assert!(names.iter().any(|name| name == "gitHead"), "{names:?}");
        assert!(
            names.iter().any(|name| name == "workspaceActivity"),
            "{names:?}"
        );
    }

    #[test]
    fn abort_edit_keeps_the_commit_until_forced() {
        let backend = FakeBackend::demo();
        let id = "demo-project";
        let sha = conflicted_sha(&backend);
        let before = backend.open_project(id).unwrap();
        backend.enter_edit_mode(id, &sha).unwrap();
        backend.edit_file(id, "src/helper.rs", "dropped\n").unwrap();
        let refused = backend
            .abort_edit_and_return_to_workspace(id, false)
            .unwrap_err();
        assert!(refused.message.contains("drop"), "{}", refused.message);
        assert!(backend.edit_status(id).unwrap().editing);

        backend
            .abort_edit_and_return_to_workspace(id, true)
            .unwrap();
        assert!(!backend.edit_status(id).unwrap().editing);
        let after = backend.open_project(id).unwrap();
        assert_eq!(after.stacks.len(), before.stacks.len());
        assert_eq!(
            lane_commits(&after, "lane-b"),
            lane_commits(&before, "lane-b")
        );
        assert!(
            !backend
                .commit_details(id, &sha)
                .unwrap()
                .message
                .contains("dropped")
        );
    }

    #[test]
    fn resolve_ours_or_theirs_clears_the_demo_conflict() {
        for choice in [ConflictChoice::Theirs, ConflictChoice::Ours] {
            let backend = FakeBackend::demo();
            let id = "demo-project";
            let sha = conflicted_sha(&backend);
            let report = backend
                .resolve_commit_conflict_hunks(
                    id,
                    &sha,
                    &[FileChoice {
                        path: "src/helper.rs".to_string(),
                        choice,
                    }],
                )
                .unwrap();
            assert_ne!(report.new_sha, sha);
            assert!(!report.still_conflicted);
            assert_eq!(report.resolved, 1);
            assert!(backend.conflicted_commits(id).unwrap().is_empty());
            let side = match choice {
                ConflictChoice::Ours => "as ours.",
                ConflictChoice::Theirs => "as theirs.",
            };
            let details = backend.commit_details(id, &report.new_sha).unwrap();
            assert!(details.message.contains(side), "{details:?}");
            assert!(
                report
                    .workspace
                    .stacks
                    .iter()
                    .flat_map(|stack| &stack.branches)
                    .flat_map(|branch| &branch.commits)
                    .all(|commit| !commit.conflicted)
            );
        }
    }

    #[test]
    fn preview_latency_delays_one_call_and_fail_is_one_shot() {
        let backend = FakeBackend::demo();
        assert_eq!(Backend::preview_latency_ms(&backend), 0);
        let started = std::time::Instant::now();
        backend.list_projects().unwrap();
        assert!(
            started.elapsed().as_millis() < 30,
            "zero latency should stay fast"
        );
        Backend::set_preview_latency_ms(&backend, 40);
        let started = std::time::Instant::now();
        backend.list_projects().unwrap();
        assert!(started.elapsed().as_millis() >= 40);
        Backend::set_preview_latency_ms(&backend, 0);
        Backend::arm_preview_fail(&backend, "push");
        let failed = backend
            .push("demo-project", "lane-c", ForcePush::No)
            .unwrap_err();
        assert!(failed.message.contains("offline"), "{}", failed.message);
        assert!(
            backend
                .push("demo-project", "lane-c", ForcePush::No)
                .is_ok()
        );
    }
}
