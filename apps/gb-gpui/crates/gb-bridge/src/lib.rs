//! Adapter over a GitButler 0.22.3 data directory.
//!
//! Listing and the workspace graph stay on a read-only sqlite connection.
//! From M1 the open project also runs GitButler's watcher
//! (`gitbutler_watcher::watch_in_background`). That watcher writes hunk
//! assignments into the project database, the same way the installed app does.
//! Mutations go through `but_api` only, one at a time per project.

mod askpass;
mod branches;
mod edit;
mod r#fake;
mod forge;
mod history;
mod live;
mod memory_meta;
mod messages;
mod mutate;
mod process;
pub mod projects_list;
mod read;
mod remote;
mod settings;

pub use askpass::{AskpassWatch, FakeAskpass};
pub use r#fake::FakeBackend;
pub use forge::{
    CiState, DisconnectedForge, FORGE_DISCONNECTED, FORGE_SIGN_IN, Forge, ForgeStatus, Review,
    ReviewState,
};
pub use messages::{
    AI_UNAVAILABLE, FakeMessageGenerator, MessageGenerator, SuggestedMessage,
    UnavailableMessageGenerator,
};
pub use process::InProcessBackend;
pub use settings::{
    FeatureFlag, GlobalPatch, GlobalSettings, ProjectSettings, Settings, SigningReport,
    removal_allowed,
};

use std::path::PathBuf;

/// Installed GitButler.app version this crate was built against.
///
/// Keep this aligned with `apps/gb-gpui/UPSTREAM.md` and the git tag in the
/// workspace `Cargo.toml`.
pub const PINNED_VERSION: &str = "0.22.3";

/// Bundle id of the installed GitButler.app. `but-path` uses it as the data
/// directory name when `CHANNEL=release`.
pub const RELEASE_IDENTIFIER: &str = "com.gitbutler.app";

/// Shown when this binary was compiled for a channel other than release.
pub const WRONG_CHANNEL_MESSAGE: &str =
    "gb-gpui was built for GitButler's dev channel; rebuild from apps/gb-gpui with CHANNEL=release";

/// How many unified-diff lines the window keeps. The row builder can make more;
/// the view asks for this cap so a huge diff stays a short vec.
pub const DIFF_ROW_CAP: usize = 4_000;

/// Data directory for the channel baked into `but-path`.
///
/// Joins the platform data directory with [`but_path::identifier`]. It does not
/// create the directory or read GitButler's files.
pub fn compiled_app_data_dir() -> PathBuf {
    but_path::app_data_dir().unwrap_or_else(|_| PathBuf::from(but_path::identifier()))
}

/// How the installed app compares with [`PINNED_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compat {
    /// Installed short version equals the pin.
    Match,
    /// An app is installed, and its short version differs.
    Mismatch { installed: String, pinned: String },
    /// No `GitButler*.app` under `/Applications` exposed a short version.
    NotInstalled,
}

/// Paths and version check. Computed without creating any GitButler file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub installed: Option<String>,
    pub pinned: String,
    pub app_data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub compat: Compat,
}

/// One row in the project list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSummary {
    pub id: String,
    pub title: String,
    pub path: String,
}

/// Result of listing projects. `projects` is empty when the list file is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectList {
    pub projects: Vec<ProjectSummary>,
    /// Why the list is empty, when GitButler has not created its files yet.
    pub reason: Option<String>,
}

/// One local commit on a lane. Upstream-only commits are not included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    pub sha: String,
    pub short_sha: String,
    pub title: String,
    pub author: String,
    pub authored_at: i64,
    pub conflicted: bool,
    pub pushed: bool,
}

/// One branch inside a stack, tip first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchSummary {
    pub name: String,
    /// First 7 hex characters, or empty when the branch has no commit.
    pub tip_sha: String,
    pub tip_full: String,
    pub commits: Vec<CommitSummary>,
}

/// One applied stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackSummary {
    pub branches: Vec<BranchSummary>,
}

impl StackSummary {
    pub fn commit_count(&self) -> usize {
        self.branches
            .iter()
            .map(|branch| branch.commits.len())
            .sum()
    }

    pub fn tip_sha(&self) -> &str {
        self.branches
            .first()
            .map(|branch| branch.tip_sha.as_str())
            .unwrap_or("")
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.branches.iter().map(|branch| branch.name.as_str())
    }
}

/// Read-only view of one project's workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub project_id: String,
    /// Full target ref, typically `refs/remotes/origin/main`.
    pub target_branch: Option<String>,
    pub stacks: Vec<StackSummary>,
}

/// What happened to one worktree or commit file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl FileStatus {
    pub fn label(self) -> &'static str {
        match self {
            FileStatus::Added => "Added",
            FileStatus::Modified => "Modified",
            FileStatus::Deleted => "Deleted",
            FileStatus::Renamed => "Renamed",
        }
    }

    pub fn mark(self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
        }
    }
}

/// One hunk and the lane it belongs to. `None` means unassigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedHunk {
    /// `@@ -old +new @@`, or empty when the file has no hunk header.
    pub header: String,
    pub lane: Option<String>,
}

/// One uncommitted file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<AssignedHunk>,
}

impl FileChange {
    /// Lane names in first-seen order. Empty when every hunk is unassigned.
    pub fn lanes(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for hunk in &self.hunks {
            if let Some(lane) = hunk.lane.as_deref()
                && !names.contains(&lane)
            {
                names.push(lane);
            }
        }
        names
    }

    /// Chip text: a lane name, `lane · N hunks`, `split`, or `Unassigned`.
    pub fn assignment_label(&self) -> String {
        let lanes = self.lanes();
        match lanes.as_slice() {
            [] => "Unassigned".to_string(),
            [lane] if self.hunks.len() <= 1 => (*lane).to_string(),
            [lane] => format!("{lane} · {} hunks", self.hunks.len()),
            _ => "split".to_string(),
        }
    }

    /// Group this file under one lane, `Split`, or `Unassigned`.
    pub fn group_name(&self) -> String {
        let lanes = self.lanes();
        match lanes.as_slice() {
            [] => "Unassigned".to_string(),
            [lane] => (*lane).to_string(),
            _ => "Split".to_string(),
        }
    }
}

/// Uncommitted files, plus the paths a watcher event said just changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeView {
    pub files: Vec<FileChange>,
    pub changed_paths: Vec<String>,
}

impl WorktreeView {
    /// Copy lane names from `previous` onto matching hunks.
    ///
    /// A refresh that returns no hunk headers would otherwise wipe lanes the
    /// watcher or a mutation already delivered.
    pub fn keep_assignments_from(&mut self, previous: &WorktreeView) {
        for file in &mut self.files {
            let Some(prior) = previous.files.iter().find(|prior| prior.path == file.path) else {
                continue;
            };
            if file.hunks.is_empty() && !prior.hunks.is_empty() {
                file.hunks = prior.hunks.clone();
                continue;
            }
            for hunk in &mut file.hunks {
                if hunk.lane.is_some() {
                    continue;
                }
                if let Some(lane) = prior
                    .hunks
                    .iter()
                    .find(|old| old.header == hunk.header)
                    .and_then(|old| old.lane.clone())
                {
                    hunk.lane = Some(lane);
                }
            }
        }
    }
}

/// A commit opened in the detail pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetails {
    pub sha: String,
    pub short_sha: String,
    pub message: String,
    pub author: String,
    pub committer: String,
    pub authored_at: i64,
    pub committed_at: i64,
    pub parents: Vec<String>,
    pub files: Vec<CommitFile>,
}

/// One file in a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    pub path: String,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
}

/// Which diff to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRequest {
    Worktree { path: String },
    Commit { sha: String, path: String },
}

/// Unified diff text, or a placeholder kind. Rows are built in the window store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDiff {
    pub path: String,
    pub patch: String,
    pub binary: bool,
    pub too_large: bool,
}

/// Something the watcher observed.
///
/// `but_api::watcher` exists only with the `export-schema` feature
/// (`crates/but-api/src/lib.rs` at release/0.22.3). [`BridgeEvent::invalidates`]
/// mirrors `WatcherEventKind::invalidates` in `crates/but-api/src/watcher.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeEvent {
    GitFetch,
    GitHead {
        head: Option<String>,
        mode: String,
    },
    GitActivity {
        head_sha: String,
    },
    WorkspaceActivity,
    /// Apply this payload. Do not re-fetch the worktree.
    Worktree(WorktreeView),
}

impl BridgeEvent {
    pub fn name(&self) -> &'static str {
        match self {
            BridgeEvent::GitFetch => "gitFetch",
            BridgeEvent::GitHead { .. } => "gitHead",
            BridgeEvent::GitActivity { .. } => "gitActivity",
            BridgeEvent::WorkspaceActivity => "workspaceActivity",
            BridgeEvent::Worktree(_) => "worktreeChanges",
        }
    }

    /// Cache tags this event makes stale. Names match `CacheTag::name()`.
    pub fn invalidates(&self) -> &'static [&'static str] {
        match self {
            BridgeEvent::GitFetch => &["Branches", "TargetCommits", "FetchStatus", "Reviews"],
            BridgeEvent::GitHead { .. } => &["OperatingMode"],
            BridgeEvent::GitActivity { .. } | BridgeEvent::WorkspaceActivity => &[
                "Branches",
                "TargetCommits",
                "Workspace",
                "Commits",
                "Diffs",
                "WorktreeChanges",
                "Worktrees",
                "AbsorptionPlan",
                "Comments",
            ],
            BridgeEvent::Worktree(_) => &["Diffs", "WorktreeChanges", "AbsorptionPlan", "Comments"],
        }
    }

    /// Git head, git activity, and workspace activity mean the commit graph moved.
    pub fn refreshes_workspace(&self) -> bool {
        matches!(
            self,
            BridgeEvent::GitActivity { .. }
                | BridgeEvent::WorkspaceActivity
                | BridgeEvent::GitHead { .. }
        )
    }
}

/// Stops the watcher and releases the project lock when dropped.
pub struct WatchHandle {
    inner: Option<WatchInner>,
    exclusive: bool,
}

enum WatchInner {
    Live {
        watcher: gitbutler_watcher::WatcherHandle,
        /// Held for the life of the watch. `None` when another GitButler process
        /// already owns the project. The window still watches.
        lock: Option<but_core::sync::LockFile>,
        task: tokio::task::JoinHandle<()>,
        /// Drops the mutation listener and the exclusive-owner count.
        cleanup: Option<Box<dyn FnOnce() + Send>>,
    },
    Fake {
        id: String,
        token: u64,
        watchers:
            std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<FakeWatch>>>>,
    },
}

struct FakeWatch {
    token: u64,
    callback: std::sync::Arc<dyn Fn(BridgeEvent) + Send + Sync>,
}

impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHandle")
            .field("exclusive", &self.exclusive)
            .field("running", &self.inner.is_some())
            .finish()
    }
}

impl WatchHandle {
    /// `true` when this process holds the project-wide lock.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    pub(crate) fn live(
        watcher: gitbutler_watcher::WatcherHandle,
        lock: Option<but_core::sync::LockFile>,
        task: tokio::task::JoinHandle<()>,
        exclusive: bool,
        cleanup: Option<Box<dyn FnOnce() + Send>>,
    ) -> Self {
        Self {
            inner: Some(WatchInner::Live {
                watcher,
                lock,
                task,
                cleanup,
            }),
            exclusive,
        }
    }

    pub(crate) fn fake(
        id: String,
        token: u64,
        watchers: std::sync::Arc<
            std::sync::Mutex<std::collections::HashMap<String, Vec<FakeWatch>>>,
        >,
        exclusive: bool,
    ) -> Self {
        Self {
            inner: Some(WatchInner::Fake {
                id,
                token,
                watchers,
            }),
            exclusive,
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        match self.inner.take() {
            Some(WatchInner::Live {
                watcher,
                lock,
                task,
                cleanup,
            }) => {
                if let Some(cleanup) = cleanup {
                    cleanup();
                }
                task.abort();
                drop(watcher);
                drop(lock);
            }
            Some(WatchInner::Fake {
                id,
                token,
                watchers,
            }) => {
                if let Ok(mut watchers) = watchers.lock()
                    && let Some(list) = watchers.get_mut(&id)
                {
                    list.retain(|watch| watch.token != token);
                }
            }
            None => {}
        }
    }
}

/// Why a bridge call failed. `message` is safe to show in the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeError {
    pub kind: BridgeErrorKind,
    pub message: String,
    /// Extra plain-English guidance. The window can show this beside `message`.
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeErrorKind {
    Compat,
    NotReady,
    NotFound,
    /// Another GitButler process holds the project lock.
    NotExclusive,
    Internal,
}

impl BridgeError {
    fn compat(message: impl Into<String>) -> Self {
        Self {
            kind: BridgeErrorKind::Compat,
            message: message.into(),
            hint: None,
        }
    }

    pub(crate) fn not_ready(message: impl Into<String>) -> Self {
        Self {
            kind: BridgeErrorKind::NotReady,
            message: message.into(),
            hint: None,
        }
    }

    /// The call is refused because this build has nothing to ask.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::not_ready(message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: BridgeErrorKind::NotFound,
            message: message.into(),
            hint: None,
        }
    }

    pub(crate) fn not_exclusive(message: impl Into<String>) -> Self {
        Self {
            kind: BridgeErrorKind::NotExclusive,
            message: message.into(),
            hint: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: BridgeErrorKind::Internal,
            message: message.into(),
            hint: None,
        }
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BridgeError {}

pub type Result<T> = std::result::Result<T, BridgeError>;

/// One hunk the window selected. Line numbers match a unified-diff header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkRef {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
}

impl HunkRef {
    /// `@@ -old,lines +new,lines @@`, the same text GitButler stores on a hunk.
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_lines, self.new_start, self.new_lines
        )
    }

    pub(crate) fn header_value(self) -> but_core::HunkHeader {
        but_core::HunkHeader {
            old_start: self.old_start,
            old_lines: self.old_lines,
            new_start: self.new_start,
            new_lines: self.new_lines,
        }
    }
}

/// Files and hunks a mutation should touch.
///
/// `hunks: None` means the whole file. `Some` is only those hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSpec {
    pub path: String,
    pub previous_path: Option<String>,
    pub hunks: Option<Vec<HunkRef>>,
}

impl SelectionSpec {
    pub fn whole(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            previous_path: None,
            hunks: None,
        }
    }

    pub fn hunks(path: impl Into<String>, hunks: Vec<HunkRef>) -> Self {
        Self {
            path: path.into(),
            previous_path: None,
            hunks: Some(hunks),
        }
    }
}

/// Where a new commit is inserted, or where a commit is moved.
///
/// `Lane` puts the commit at the tip of that lane (below the branch ref).
/// `Commit { below: true }` inserts on the base side of that commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    Lane(String),
    Commit { sha: String, below: bool },
}

/// A change GitButler refused to apply. `reason` is plain English.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedChange {
    pub path: String,
    pub reason: String,
}

/// Workspace after a commit mutation. `new_sha` is set when the call created one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitResult {
    pub workspace: WorkspaceSummary,
    pub new_sha: Option<String>,
    pub rejected: Vec<RejectedChange>,
}

/// Workspace after a discard. `refused` lists paths GitButler could not drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardResult {
    pub workspace: WorkspaceSummary,
    pub refused: Vec<String>,
}

/// Workspace after a new lane is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCreated {
    pub workspace: WorkspaceSummary,
    pub name: String,
}

/// One oplog snapshot, newest first when listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub sha: String,
    /// Unix time in milliseconds.
    pub created_at_ms: i128,
    /// GitButler's persisted operation name, such as `CreateCommit`.
    pub operation: String,
    /// Short label, such as `Created commit`. A manual snapshot uses its message.
    pub title: String,
    /// False only for undo and redo bookkeeping entries.
    pub restorable: bool,
}

/// One file a snapshot's operation changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotChange {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
}

/// What a commit hook reported. This crate does not run hooks itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookStatus {
    Success,
    NotConfigured,
    Failed {
        error: String,
    },
    /// The message hook rewrote the commit message.
    Rewritten {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub status: HookStatus,
}

/// Workspace after undo or an explicit snapshot restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoResult {
    pub workspace: WorkspaceSummary,
    pub restored_sha: String,
}

/// Where a listed branch lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchPlace {
    Local,
    Remote,
    Both,
}

/// One row on the branches screen. Local and remote refs of the same name are one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    pub name: String,
    /// Full ref, such as `refs/heads/side` or `refs/remotes/origin/topic`.
    pub ref_name: String,
    pub place: BranchPlace,
    /// Full remote-tracking refs that share this name.
    pub remotes: Vec<String>,
    pub applied: bool,
    /// `applied`, `unapplied`, `target`, or `standalone`.
    pub stack: String,
    /// Branch names in this stack, tip first.
    pub stack_branches: Vec<String>,
    pub tip_sha: String,
    pub tip_short: String,
    pub last_commit: String,
    /// `Name <email>`, or empty when the tip has no author.
    pub author: String,
    pub updated_at_ms: Option<i64>,
    /// Commits on this branch that are not on the target. `None` when there is no target.
    pub ahead: Option<usize>,
    /// Commits on the target that are not on this branch. `None` when there is no target.
    pub behind: Option<usize>,
}

/// Commits on one branch, plus how far it is from the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDetails {
    pub name: String,
    pub ref_name: String,
    pub applied: bool,
    pub commits: Vec<CommitSummary>,
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
}

/// Workspace after a branch was applied, or found already applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    pub workspace: WorkspaceSummary,
    /// Short names of the branches this call applied. Empty when nothing changed.
    pub names: Vec<String>,
    pub already_applied: bool,
}

/// Whether this project is in edit mode, and which commit is checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditStatus {
    pub editing: bool,
    pub commit_sha: Option<String>,
    pub stack_id: Option<String>,
    /// Short `HEAD` name, such as `gitbutler/edit` or `gitbutler/workspace`.
    pub head: Option<String>,
    /// Index conflicts of the edit session. Empty when not editing.
    pub conflicted_files: Vec<String>,
}

/// A workspace commit GitButler has marked conflicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedCommit {
    pub sha: String,
    pub short_sha: String,
    pub title: String,
    pub author: String,
    pub lane: String,
}

/// Which side of a conflicted file to keep. One choice covers every hunk in that file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    /// The new base the commit was rebased onto.
    Ours,
    /// The commit's own version.
    Theirs,
}

/// A per-file choice for [`Backend::resolve_commit_conflict_hunks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChoice {
    pub path: String,
    pub choice: ConflictChoice,
}

/// Workspace after conflict hunks were resolved. The commit id changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveReport {
    pub previous_sha: String,
    pub new_sha: String,
    pub resolved: usize,
    pub still_conflicted: bool,
    pub workspace: WorkspaceSummary,
}

/// Shown when a write is attempted while a commit is checked out for editing.
pub(crate) const EDIT_MODE_MESSAGE: &str =
    "Leave edit mode before changing this project. Save the edit or abort it first.";

/// A remote-tracking branch the setup screen can pick as the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: String,
    pub branch: String,
    /// Full ref, such as `refs/remotes/origin/main`.
    pub full_name: String,
}

/// One commit that is on the target branch and not in this workspace yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamCommit {
    pub sha: String,
    pub short_sha: String,
    pub title: String,
    pub author: String,
}

/// The project's target branch, push remote, and how far behind upstream it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseBranchData {
    pub branch_name: String,
    pub short_name: String,
    pub remote_name: String,
    pub remote_url: String,
    pub push_remote_name: String,
    pub push_remote_url: String,
    pub base_sha: String,
    pub current_sha: String,
    pub behind: usize,
    pub upstream_commits: Vec<UpstreamCommit>,
    /// Timestamp from the project's last fetch record, when GitButler stored one.
    pub last_fetched_ms: Option<u128>,
    pub conflicted: bool,
}

/// How many new upstream commits a fetch left, plus when the fetch ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchStatus {
    pub upstream_count: usize,
    pub summary: String,
    /// `workspace_fetch_status` last success, in milliseconds. This is the
    /// "last fetched" value the window shows.
    pub last_fetched_ms: Option<u128>,
    pub last_attempted_ms: Option<u128>,
    pub last_error: Option<String>,
    pub commits: Vec<UpstreamCommit>,
}

impl FetchStatus {
    pub fn summary_for(count: usize) -> String {
        match count {
            0 => "Up to date".to_string(),
            1 => "1 new upstream commit".to_string(),
            n => format!("{n} new upstream commits"),
        }
    }
}

/// Result of fetching remotes. Git does not stream progress into this crate,
/// so `steps` is the outcome in plain English.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    pub status: FetchStatus,
    pub steps: Vec<String>,
}

/// How to move each lane when integrating upstream commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStrategy {
    Rebase,
    Merge,
}

/// What a dry run says will happen to one lane.
///
/// A lane missing from the preview was integrated. A lane whose preview commits
/// conflict is `Conflicted`. Anything else is `Clear`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrateResolution {
    Clear,
    Integrated,
    Conflicted,
}

/// One lane in an integrate preview or apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneResolution {
    pub lane: String,
    pub strategy: UpstreamStrategy,
    pub resolution: IntegrateResolution,
    /// Commit titles, tip first. Empty when the lane was integrated away.
    pub commits: Vec<String>,
}

/// Dry-run of an upstream integrate. Nothing is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegratePreview {
    pub resolutions: Vec<LaneResolution>,
    pub worktree_conflicts: Vec<String>,
    pub summary: String,
}

/// Workspace after an upstream integrate was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrateReport {
    pub workspace: WorkspaceSummary,
    pub resolutions: Vec<LaneResolution>,
    pub worktree_conflicts: Vec<String>,
}

/// How hard a push may update the remote.
///
/// GitButler's project setting `force_push_protection` defaults to off, so
/// [`ForcePush::WithLease`] and [`ForcePush::Force`] send the same git
/// arguments until that setting is turned on. With it on, `WithLease` is
/// `--force-with-lease` plus `--force-if-includes`, and `Force` is `--force`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcePush {
    /// Regular push. Rejected when the remote is not a fast-forward.
    No,
    /// Request a force update and leave force-push protection enabled.
    ///
    /// When the project protects force pushes, a stale remote-tracking ref is
    /// rejected. When protection is off, this is an ordinary `--force`.
    WithLease,
    /// Request a force update and skip force-push protection. Always `--force`.
    Force,
}

/// One branch a push updated on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedBranch {
    pub name: String,
    pub remote_ref: String,
    pub remote_branch: String,
    pub before_sha: String,
    pub after_sha: String,
}

/// Result of pushing a lane. `steps` describes the outcome. A rejected push
/// is an error, not a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushReport {
    pub remote: String,
    pub branches: Vec<PushedBranch>,
    pub summary: String,
    pub force: ForcePush,
    pub steps: Vec<String>,
}

/// What Git is asking the user to type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskpassKind {
    Credential,
    Passphrase,
    Other,
}

/// A credential prompt the window can show. `prompt` is Git's question.
/// The answer is never stored on this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskpassRequest {
    pub id: String,
    pub prompt: String,
    pub kind: AskpassKind,
}

/// A prompt answer. `Debug` and `Display` never include the text.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Option<String>);

impl Secret {
    pub fn new(value: Option<String>) -> Self {
        Self(value)
    }

    /// Only the askpass broker reads this, at the moment it submits the answer.
    pub fn into_inner(self) -> Option<String> {
        self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(_) => formatter.write_str("Secret([redacted])"),
            None => formatter.write_str("Secret(None)"),
        }
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, formatter)
    }
}

/// GitButler access. `InProcessBackend` talks to the real files.
/// `FakeBackend` serves canned data for the window and its tests.
pub trait Backend: Settings + Send + Sync {
    fn environment(&self) -> Result<Environment>;
    fn list_projects(&self) -> Result<ProjectList>;
    fn open_project(&self, id: &str) -> Result<WorkspaceSummary>;
    fn worktree(&self, id: &str) -> Result<WorktreeView> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not read uncommitted changes",
        ))
    }
    fn commit_details(&self, id: &str, sha: &str) -> Result<CommitDetails> {
        let _ = (id, sha);
        Err(BridgeError::not_ready("This backend does not read commits"))
    }
    fn file_diff(&self, id: &str, request: DiffRequest) -> Result<TextDiff> {
        let _ = (id, request);
        Err(BridgeError::not_ready("This backend does not read diffs"))
    }
    fn watch(
        &self,
        id: &str,
        on_change: Box<dyn Fn(BridgeEvent) + Send + Sync>,
    ) -> Result<WatchHandle>;
    /// Canned data. The real backend refuses this.
    fn is_preview(&self) -> bool {
        false
    }
    fn trigger_fake(&self, id: &str, kind: &str) -> Result<()> {
        let _ = (id, kind);
        Err(BridgeError::not_found(
            "trigger-event works on the preview backend only",
        ))
    }

    /// Assign the selection to `lane`, or clear the assignment when `lane` is `None`.
    fn assign(
        &self,
        id: &str,
        selections: &[SelectionSpec],
        lane: Option<&str>,
    ) -> Result<WorkspaceSummary> {
        let _ = (id, selections, lane);
        Err(BridgeError::not_ready(
            "This backend does not assign changes",
        ))
    }

    fn discard(&self, id: &str, selections: &[SelectionSpec]) -> Result<DiscardResult> {
        let _ = (id, selections);
        Err(BridgeError::not_ready(
            "This backend does not discard changes",
        ))
    }

    fn create_lane(&self, id: &str, name: &str) -> Result<LaneCreated> {
        let _ = (id, name);
        Err(BridgeError::not_ready("This backend does not create lanes"))
    }

    fn commit_create(
        &self,
        id: &str,
        anchor: &Anchor,
        selections: &[SelectionSpec],
        message: &str,
    ) -> Result<CommitResult> {
        let _ = (id, anchor, selections, message);
        Err(BridgeError::not_ready(
            "This backend does not create commits",
        ))
    }

    fn amend(&self, id: &str, sha: &str, selections: &[SelectionSpec]) -> Result<CommitResult> {
        let _ = (id, sha, selections);
        Err(BridgeError::not_ready(
            "This backend does not amend commits",
        ))
    }

    fn reword(&self, id: &str, sha: &str, message: &str) -> Result<CommitResult> {
        let _ = (id, sha, message);
        Err(BridgeError::not_ready(
            "This backend does not reword commits",
        ))
    }

    fn squash(&self, id: &str, subject_shas: &[String], target_sha: &str) -> Result<CommitResult> {
        let _ = (id, subject_shas, target_sha);
        Err(BridgeError::not_ready(
            "This backend does not squash commits",
        ))
    }

    fn uncommit(&self, id: &str, shas: &[String], lane: Option<&str>) -> Result<CommitResult> {
        let _ = (id, shas, lane);
        Err(BridgeError::not_ready("This backend does not uncommit"))
    }

    fn uncommit_changes(
        &self,
        id: &str,
        sha: &str,
        selections: &[SelectionSpec],
        lane: Option<&str>,
    ) -> Result<CommitResult> {
        let _ = (id, sha, selections, lane);
        Err(BridgeError::not_ready(
            "This backend does not uncommit changes",
        ))
    }

    fn move_commit(&self, id: &str, shas: &[String], anchor: &Anchor) -> Result<CommitResult> {
        let _ = (id, shas, anchor);
        Err(BridgeError::not_ready("This backend does not move commits"))
    }

    fn insert_blank(&self, id: &str, anchor: &Anchor) -> Result<CommitResult> {
        let _ = (id, anchor);
        Err(BridgeError::not_ready(
            "This backend does not insert blank commits",
        ))
    }

    fn move_changes(
        &self,
        id: &str,
        source_sha: &str,
        destination_sha: &str,
        selections: &[SelectionSpec],
    ) -> Result<CommitResult> {
        let _ = (id, source_sha, destination_sha, selections);
        Err(BridgeError::not_ready(
            "This backend does not move changes between commits",
        ))
    }

    fn snapshots(&self, id: &str, limit: usize) -> Result<Vec<SnapshotInfo>> {
        let _ = (id, limit);
        Err(BridgeError::not_ready(
            "This backend does not list snapshots",
        ))
    }

    fn restore_snapshot(&self, id: &str, sha: &str) -> Result<UndoResult> {
        let _ = (id, sha);
        Err(BridgeError::not_ready(
            "This backend does not restore snapshots",
        ))
    }

    fn undo_last(&self, id: &str) -> Result<UndoResult> {
        let _ = id;
        Err(BridgeError::not_ready("This backend does not undo"))
    }

    /// Same list as [`Self::snapshots`]. The default calls that method.
    fn list_snapshots(&self, id: &str, limit: usize) -> Result<Vec<SnapshotInfo>> {
        self.snapshots(id, limit)
    }

    /// Files the snapshot's operation changed, compared with the following snapshot.
    fn snapshot_diff(&self, id: &str, sha: &str) -> Result<Vec<SnapshotChange>> {
        let _ = (id, sha);
        Err(BridgeError::not_ready(
            "This backend does not diff snapshots",
        ))
    }

    /// Record the project as it is now. `message` is the label.
    fn create_snapshot(&self, id: &str, message: &str) -> Result<SnapshotInfo> {
        let _ = (id, message);
        Err(BridgeError::not_ready(
            "This backend does not create snapshots",
        ))
    }

    /// Local and remote branches. Applied lanes are included.
    ///
    /// A remote branch is named with its full ref (`refs/remotes/origin/topic`).
    /// A short name means a local branch.
    fn list_branches(&self, id: &str) -> Result<Vec<BranchRow>> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not list branches",
        ))
    }

    fn branch_details(&self, id: &str, ref_name: &str) -> Result<BranchDetails> {
        let _ = (id, ref_name);
        Err(BridgeError::not_ready(
            "This backend does not read branch commits",
        ))
    }

    /// Apply `ref_name` to the workspace as a stack. A conflicting apply is an error.
    fn apply_branch(&self, id: &str, ref_name: &str) -> Result<ApplyResult> {
        let _ = (id, ref_name);
        Err(BridgeError::not_ready(
            "This backend does not apply branches",
        ))
    }

    /// Take the stack named `name` out of the workspace. The git ref stays.
    fn unapply_branch(&self, id: &str, name: &str) -> Result<WorkspaceSummary> {
        let _ = (id, name);
        Err(BridgeError::not_ready(
            "This backend does not unapply branches",
        ))
    }

    /// Delete a local branch that is not applied. An applied branch is refused.
    fn delete_local_branch(&self, id: &str, name: &str) -> Result<WorkspaceSummary> {
        let _ = (id, name);
        Err(BridgeError::not_ready(
            "This backend does not delete branches",
        ))
    }

    fn edit_status(&self, id: &str) -> Result<EditStatus> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not read edit mode",
        ))
    }

    /// Check out `sha` for editing. The commit must be in the workspace.
    fn enter_edit_mode(&self, id: &str, sha: &str) -> Result<EditStatus> {
        let _ = (id, sha);
        Err(BridgeError::not_ready(
            "This backend does not enter edit mode",
        ))
    }

    fn save_edit_and_return_to_workspace(&self, id: &str) -> Result<WorkspaceSummary> {
        let _ = id;
        Err(BridgeError::not_ready("This backend does not save an edit"))
    }

    /// Leave edit mode. `force` drops uncommitted edits. Without it, a dirty
    /// edit is refused and the session stays open.
    fn abort_edit_and_return_to_workspace(
        &self,
        id: &str,
        force: bool,
    ) -> Result<WorkspaceSummary> {
        let _ = (id, force);
        Err(BridgeError::not_ready(
            "This backend does not abort an edit",
        ))
    }

    fn conflicted_commits(&self, id: &str) -> Result<Vec<ConflictedCommit>> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not list conflicted commits",
        ))
    }

    /// Resolve every hunk of each file as ours or theirs. The commit id changes.
    fn resolve_commit_conflict_hunks(
        &self,
        id: &str,
        sha: &str,
        files: &[FileChoice],
    ) -> Result<ResolveReport> {
        let _ = (id, sha, files);
        Err(BridgeError::not_ready(
            "This backend does not resolve conflicts",
        ))
    }

    fn pre_commit_hook(&self, id: &str, selections: &[SelectionSpec]) -> Result<HookReport> {
        let _ = (id, selections);
        Err(BridgeError::not_ready(
            "This backend does not read commit hooks",
        ))
    }

    fn post_commit_hook(&self, id: &str) -> Result<HookReport> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not read commit hooks",
        ))
    }

    fn message_hook(&self, id: &str, message: &str) -> Result<HookReport> {
        let _ = (id, message);
        Err(BridgeError::not_ready(
            "This backend does not read commit hooks",
        ))
    }

    /// Remember an existing repository. Settings must already exist.
    /// The same path twice returns the project that is already remembered.
    fn add_project(&self, path: &str) -> Result<ProjectSummary> {
        let _ = path;
        Err(BridgeError::not_ready("This backend does not add projects"))
    }

    /// Remove the project from GitButler's list. The worktree stays on disk.
    fn delete_project(&self, id: &str) -> Result<()> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not remove projects",
        ))
    }

    fn base_branch(&self, id: &str) -> Result<Option<BaseBranchData>> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not read the target branch",
        ))
    }

    fn remote_branches(&self, id: &str) -> Result<Vec<RemoteBranch>> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not list remote branches",
        ))
    }

    /// Point the project at a remote-tracking branch. Does not create lanes.
    /// An empty `push_remote` leaves the current push remote alone.
    fn set_target(
        &self,
        id: &str,
        target_ref: &str,
        push_remote: Option<&str>,
    ) -> Result<BaseBranchData> {
        let _ = (id, target_ref, push_remote);
        Err(BridgeError::not_ready(
            "This backend does not set the target branch",
        ))
    }

    fn fetch(&self, id: &str) -> Result<FetchReport> {
        let _ = id;
        Err(BridgeError::not_ready("This backend does not fetch"))
    }

    fn fetch_status(&self, id: &str) -> Result<FetchStatus> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not read fetch status",
        ))
    }

    fn integrate_preview(&self, id: &str, strategy: UpstreamStrategy) -> Result<IntegratePreview> {
        let _ = (id, strategy);
        Err(BridgeError::not_ready(
            "This backend does not preview upstream integration",
        ))
    }

    fn integrate(&self, id: &str, strategy: UpstreamStrategy) -> Result<IntegrateReport> {
        let _ = (id, strategy);
        Err(BridgeError::not_ready(
            "This backend does not integrate upstream commits",
        ))
    }

    fn push(&self, id: &str, lane: &str, force: ForcePush) -> Result<PushReport> {
        let _ = (id, lane, force);
        Err(BridgeError::not_ready("This backend does not push"))
    }

    /// Prompts waiting for an answer. An empty `Ok` means nothing is waiting.
    /// The default is an error so a backend that forgot the method is obvious.
    fn askpass_pending(&self) -> Result<Vec<AskpassRequest>> {
        Err(BridgeError::not_ready(
            "This backend does not broker credential prompts",
        ))
    }

    fn watch_askpass(
        &self,
        on_prompt: Box<dyn Fn(AskpassRequest) + Send + Sync>,
    ) -> Result<AskpassWatch> {
        let _ = on_prompt;
        Err(BridgeError::not_ready(
            "This backend does not broker credential prompts",
        ))
    }

    /// `None` cancels the prompt. The text is not written to a log.
    fn answer_askpass(&self, id: &str, response: Option<String>) -> Result<()> {
        let _ = (id, Secret::new(response));
        Err(BridgeError::not_ready(
            "This backend does not broker credential prompts",
        ))
    }

    /// Pull requests for this project. The default is the disconnected status.
    fn forge_status(&self, id: &str) -> Result<ForgeStatus> {
        crate::forge::DisconnectedForge.reviews(id)
    }

    fn review_for_branch(&self, id: &str, branch: &str) -> Result<Option<Review>> {
        crate::forge::DisconnectedForge.review_for_branch(id, branch)
    }

    /// Open a draft pull request. The default refuses without contacting a forge.
    fn publish_draft(&self, id: &str, branch: &str) -> Result<Review> {
        crate::forge::DisconnectedForge.publish_draft(id, branch)
    }

    /// Suggest a commit message for `paths`. The default does not contact a model.
    fn suggest_message(&self, id: &str, paths: &[String]) -> Result<SuggestedMessage> {
        let _ = id;
        crate::messages::UnavailableMessageGenerator.suggest(paths)
    }

    /// Extra wait the fake backend applies before each gated call. Live stays at 0.
    fn preview_latency_ms(&self) -> u64 {
        0
    }

    /// Preview only. The live backend ignores this.
    fn set_preview_latency_ms(&self, _ms: u64) {}

    /// Preview only. The next call of `op` fails once, before any delay.
    fn arm_preview_fail(&self, _op: &str) {}
}

fn compat_error(env: &Environment) -> BridgeError {
    match &env.compat {
        Compat::Mismatch { installed, pinned } => BridgeError::compat(format!(
            "GitButler {installed} is installed, gb-gpui is built for {pinned}"
        )),
        Compat::NotInstalled => BridgeError::compat(format!(
            "GitButler is not installed, gb-gpui is built for {}",
            env.pinned
        )),
        Compat::Match => BridgeError::compat("versions match"),
    }
}

pub(crate) fn short_sha(id: gix::ObjectId) -> String {
    let hex = id.to_hex();
    let text = hex.to_string();
    text.chars().take(7).collect()
}

pub(crate) fn branch_label(name: &gix::refs::FullNameRef) -> String {
    let short = name.shorten().to_string();
    if short.is_empty() {
        name.to_string()
    } else {
        short
    }
}

pub(crate) fn lossy_bytes(bytes: impl AsRef<[u8]>) -> String {
    String::from_utf8_lossy(bytes.as_ref()).into_owned()
}

pub(crate) fn first_line(bytes: impl AsRef<[u8]>) -> String {
    lossy_bytes(bytes)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}
