//! Branches page, history, edit mode, and conflict resolution.
//!
//! No GPUI types. The window runs a [`PageOp`] through [`perform_page`], then
//! [`Store::accept_page`]. Filtering, snapshot selection, the edit-mode state
//! machine, and the conflict plan are pure so they can be tested on their own.

use std::sync::Arc;

use gb_bridge::{
    Backend, BranchPlace, BranchRow, ConflictChoice, EditStatus, FileChoice, FileStatus,
    SnapshotChange, SnapshotInfo, WorkspaceSummary, WorktreeView,
};

use super::{Effect, ShownFile, SideFocus, Store};

/// Shown on every disabled mutation control while a commit is checked out.
pub const EDIT_LOCK: &str =
    "Leave edit mode before changing this project. Save the edit or abort it first.";

const SNAPSHOT_LIMIT: usize = 50;

/// Local, remote, or pull requests from the in-process forge picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchTab {
    Local,
    Remote,
    PullRequests,
}

impl BranchTab {
    pub fn name(self) -> &'static str {
        match self {
            BranchTab::Local => "local",
            BranchTab::Remote => "remote",
            BranchTab::PullRequests => "pr",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BranchTab::Local => "Local",
            BranchTab::Remote => "Remote",
            BranchTab::PullRequests => "Pull requests",
        }
    }

    #[cfg(feature = "devtools")]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "local" => Some(BranchTab::Local),
            "remote" => Some(BranchTab::Remote),
            "pr" | "pull" | "pull-requests" => Some(BranchTab::PullRequests),
            _ => None,
        }
    }

    fn next(self, step: i32) -> Self {
        let order = [BranchTab::Local, BranchTab::Remote, BranchTab::PullRequests];
        let index = order.iter().position(|tab| *tab == self).unwrap_or(0) as i32;
        let next = (index + step).rem_euclid(order.len() as i32) as usize;
        order[next]
    }
}

/// Where an edit session is. Illegal jumps are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditPhase {
    #[default]
    Idle,
    Entering,
    Editing,
    Saving,
    Aborting,
}

/// What the user or the bridge just did to an edit session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditEvent {
    Enter,
    Entered,
    Save,
    Abort,
    Finished,
    Failed,
}

/// One file in the conflict panel. `side` stays empty until the user picks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: String,
    pub side: Option<ConflictChoice>,
}

/// A confirm sitting on the branches page or the history panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageAsk {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub destructive: bool,
    pub op: PageOp,
}

/// One bridge call for these screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageOp {
    ListBranches,
    Apply { name: String, ref_name: String },
    Unapply { name: String },
    Delete { name: String },
    ListSnapshots,
    SnapshotDiff { sha: String },
    CreateSnapshot { message: String },
    Restore { sha: String },
    Enter { sha: String },
    SaveEdit,
    AbortEdit,
    ConflictFiles { sha: String },
    Resolve { sha: String, files: Vec<FileChoice> },
}

pub(crate) fn page_activity(op: &PageOp) -> (&'static str, Option<String>) {
    match op {
        PageOp::ListBranches => ("list_branches", None),
        PageOp::Apply { name, .. } => ("apply_branch", Some(name.clone())),
        PageOp::Unapply { name } => ("unapply_branch", Some(name.clone())),
        PageOp::Delete { name } => ("delete_local_branch", Some(name.clone())),
        PageOp::ListSnapshots => ("list_snapshots", None),
        PageOp::SnapshotDiff { sha } => ("snapshot_diff", Some(sha.clone())),
        PageOp::CreateSnapshot { .. } => ("create_snapshot", None),
        PageOp::Restore { sha } => ("restore_snapshot", Some(sha.clone())),
        PageOp::Enter { sha } => ("enter_edit_mode", Some(sha.clone())),
        PageOp::SaveEdit => ("save_edit_and_return_to_workspace", None),
        PageOp::AbortEdit => ("abort_edit_and_return_to_workspace", None),
        PageOp::ConflictFiles { sha } => ("conflicted_commits", Some(sha.clone())),
        PageOp::Resolve { sha, .. } => ("resolve_commit_conflict_hunks", Some(sha.clone())),
    }
}

fn page_done_id(done: &PageDone) -> &'static str {
    match done {
        PageDone::Branches(_) => "list_branches",
        PageDone::Applied { .. } => "apply_branch",
        PageDone::Unapplied { .. } => "unapply_branch",
        PageDone::Deleted { .. } => "delete_local_branch",
        PageDone::Snapshots(_) => "list_snapshots",
        PageDone::Diff { .. } => "snapshot_diff",
        PageDone::Created(_) => "create_snapshot",
        PageDone::Restored { .. } => "restore_snapshot",
        PageDone::Editing(_) => "enter_edit_mode",
        PageDone::Saved { .. } => "save_edit_and_return_to_workspace",
        PageDone::Aborted { .. } => "abort_edit_and_return_to_workspace",
        PageDone::ConflictFiles { .. } => "conflicted_commits",
        PageDone::Resolved { .. } => "resolve_commit_conflict_hunks",
    }
}

/// What the bridge returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageDone {
    Branches(Vec<BranchRow>),
    Applied {
        name: String,
        workspace: WorkspaceSummary,
        worktree: WorktreeView,
        already: bool,
    },
    Unapplied {
        name: String,
        workspace: WorkspaceSummary,
        worktree: WorktreeView,
    },
    Deleted {
        name: String,
        workspace: WorkspaceSummary,
        worktree: WorktreeView,
    },
    Snapshots(Vec<SnapshotInfo>),
    Diff {
        sha: String,
        files: Vec<SnapshotChange>,
    },
    Created(SnapshotInfo),
    Restored {
        workspace: WorkspaceSummary,
        worktree: WorktreeView,
        sha: String,
    },
    Editing(EditStatus),
    Saved {
        workspace: WorkspaceSummary,
        worktree: WorktreeView,
    },
    Aborted {
        workspace: WorkspaceSummary,
        worktree: WorktreeView,
    },
    ConflictFiles {
        sha: String,
        title: String,
        paths: Vec<String>,
    },
    Resolved {
        workspace: WorkspaceSummary,
        worktree: WorktreeView,
        previous: String,
        new_sha: String,
    },
}

/// Which topbar tab is showing. Settings and the keyboard card stay overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPage {
    Workspace,
    Branches,
    History,
}

impl AppPage {
    pub fn name(self) -> &'static str {
        match self {
            AppPage::Workspace => "workspace",
            AppPage::Branches => "branches",
            AppPage::History => "history",
        }
    }

    #[cfg(feature = "devtools")]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "workspace" => Some(Self::Workspace),
            "branches" => Some(Self::Branches),
            "history" => Some(Self::History),
            _ => None,
        }
    }
}

/// Branches, history, edit mode, and the conflict panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pages {
    pub branches_open: bool,
    pub tab: BranchTab,
    pub filter: String,
    pub branch_rows: Vec<BranchRow>,
    pub branch_cursor: usize,
    /// Vertical scroll of the branches list, in points. Switching pages keeps it.
    pub branch_scroll: i32,
    pub branches_loading: bool,
    pub branches_error: Option<String>,
    pub branch_notice: Option<String>,
    pub branch_notice_bad: bool,
    pub branch_busy: Option<String>,
    pub history_open: bool,
    pub snapshots: Vec<SnapshotInfo>,
    pub snapshot_cursor: usize,
    /// Vertical scroll of the snapshot list, in points. Switching pages keeps it.
    pub history_scroll: i32,
    pub snapshot_sha: Option<String>,
    /// Shared with the window so a frame never clones the list.
    pub snapshot_files: Arc<Vec<SnapshotChange>>,
    pub history_loading: bool,
    pub diff_loading: bool,
    pub history_error: Option<String>,
    pub history_notice: Option<String>,
    pub history_notice_bad: bool,
    pub snapshot_message: String,
    pub message_focused: bool,
    pub history_busy: bool,
    pub phase: EditPhase,
    pub edit_sha: Option<String>,
    pub edit_short: Option<String>,
    pub edit_error: Option<String>,
    pub conflicts_open: bool,
    pub conflict_sha: Option<String>,
    pub conflict_title: String,
    pub conflict_files: Vec<ConflictFile>,
    pub conflict_cursor: usize,
    pub conflict_busy: bool,
    pub conflict_notice: Option<String>,
    pub conflict_notice_bad: bool,
    pub ask: Option<PageAsk>,
}

impl Default for Pages {
    fn default() -> Self {
        Self {
            branches_open: false,
            tab: BranchTab::Local,
            filter: String::new(),
            branch_rows: Vec::new(),
            branch_cursor: 0,
            branch_scroll: 0,
            branches_loading: false,
            branches_error: None,
            branch_notice: None,
            branch_notice_bad: false,
            branch_busy: None,
            history_open: false,
            snapshots: Vec::new(),
            snapshot_cursor: 0,
            history_scroll: 0,
            snapshot_sha: None,
            snapshot_files: Arc::new(Vec::new()),
            history_loading: false,
            diff_loading: false,
            history_error: None,
            history_notice: None,
            history_notice_bad: false,
            snapshot_message: String::new(),
            message_focused: false,
            history_busy: false,
            phase: EditPhase::Idle,
            edit_sha: None,
            edit_short: None,
            edit_error: None,
            conflicts_open: false,
            conflict_sha: None,
            conflict_title: String::new(),
            conflict_files: Vec::new(),
            conflict_cursor: 0,
            conflict_busy: false,
            conflict_notice: None,
            conflict_notice_bad: false,
            ask: None,
        }
    }
}

impl Pages {
    fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn editing(&self) -> bool {
        self.phase != EditPhase::Idle
    }

    pub fn visible_branches(&self) -> Vec<BranchRow> {
        visible_indices(&self.branch_rows, self.tab, &self.filter)
            .into_iter()
            .filter_map(|index| self.branch_rows.get(index).cloned())
            .collect()
    }

    pub fn selected_branch(&self) -> Option<&BranchRow> {
        let indexes = visible_indices(&self.branch_rows, self.tab, &self.filter);
        indexes
            .get(self.branch_cursor)
            .and_then(|index| self.branch_rows.get(*index))
    }

    pub fn selected_snapshot(&self) -> Option<&SnapshotInfo> {
        self.snapshots.get(self.snapshot_cursor)
    }

    pub fn conflict_file(&self) -> Option<&ConflictFile> {
        self.conflict_files.get(self.conflict_cursor)
    }
}

/// Advance the edit session. A jump that skips a step is an error.
pub fn step_edit(phase: EditPhase, event: EditEvent) -> Result<EditPhase, &'static str> {
    let next = match (phase, event) {
        (EditPhase::Idle, EditEvent::Enter) => EditPhase::Entering,
        (EditPhase::Entering, EditEvent::Entered) => EditPhase::Editing,
        (EditPhase::Entering, EditEvent::Failed) => EditPhase::Idle,
        (EditPhase::Editing, EditEvent::Save) => EditPhase::Saving,
        (EditPhase::Editing, EditEvent::Abort) => EditPhase::Aborting,
        (EditPhase::Saving | EditPhase::Aborting, EditEvent::Finished) => EditPhase::Idle,
        (EditPhase::Saving | EditPhase::Aborting, EditEvent::Failed) => EditPhase::Editing,
        _ => return Err("That edit action is not available now."),
    };
    Ok(next)
}

/// Indexes of rows on `tab` whose name, commit, author, or ref contains `query`.
pub fn visible_indices(rows: &[BranchRow], tab: BranchTab, query: &str) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| branch_on_tab(row, tab) && branch_matches(row, query))
        .map(|(index, _)| index)
        .collect()
}

pub fn branch_on_tab(row: &BranchRow, tab: BranchTab) -> bool {
    match tab {
        BranchTab::Local => matches!(row.place, BranchPlace::Local | BranchPlace::Both),
        BranchTab::Remote => {
            matches!(row.place, BranchPlace::Remote | BranchPlace::Both) || !row.remotes.is_empty()
        }
        BranchTab::PullRequests => false,
    }
}

pub fn branch_matches(row: &BranchRow, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    [
        &row.name,
        &row.last_commit,
        &row.author,
        &row.ref_name,
        &row.tip_short,
    ]
    .into_iter()
    .any(|field| field.to_ascii_lowercase().contains(&query))
}

/// `id` is a full sha, a short prefix, or a list index.
pub fn find_snapshot(snapshots: &[SnapshotInfo], id: &str) -> Option<usize> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    if let Ok(index) = id.parse::<usize>()
        && index < snapshots.len()
        && !snapshots
            .iter()
            .any(|snapshot| snapshot.sha.starts_with(id))
    {
        return Some(index);
    }
    snapshots.iter().position(|snapshot| {
        snapshot.sha == id || snapshot.sha.starts_with(id) || short7(&snapshot.sha) == id
    })
}

/// Every file needs a side. An empty list cannot be marked resolved.
pub fn resolution_plan(files: &[ConflictFile]) -> Result<Vec<FileChoice>, &'static str> {
    if files.is_empty() {
        return Err("This commit has no files to resolve.");
    }
    if files.iter().any(|file| file.side.is_none()) {
        return Err("Choose ours or theirs for every file.");
    }
    Ok(files
        .iter()
        .map(|file| FileChoice {
            path: file.path.clone(),
            choice: file.side.expect("checked"),
        })
        .collect())
}

pub fn set_conflict_side(
    files: &mut [ConflictFile],
    path: &str,
    side: ConflictChoice,
) -> Result<(), String> {
    let Some(file) = files.iter_mut().find(|file| file.path == path) else {
        return Err(format!("No conflict in {path}."));
    };
    file.side = Some(side);
    Ok(())
}

pub fn parse_side(side: &str) -> Result<ConflictChoice, String> {
    match side {
        "ours" | "our" | "base" => Ok(ConflictChoice::Ours),
        "theirs" | "their" | "commit" => Ok(ConflictChoice::Theirs),
        other => Err(format!("side is ours or theirs, got {other}")),
    }
}

#[cfg(any(test, feature = "devtools"))]
pub fn side_name(side: ConflictChoice) -> &'static str {
    match side {
        ConflictChoice::Ours => "ours",
        ConflictChoice::Theirs => "theirs",
    }
}

pub fn snapshot_kind(operation: &str) -> &'static str {
    match operation {
        "OnDemandSnapshot" => "Snapshot",
        "CreateCommit" | "AmendCommit" | "RewordCommit" => "Commit",
        "CreateBranch" => "Branch",
        "UnapplyBranch" => "Unapply",
        "DeleteBranch" => "Delete",
        "RestoreFromSnapshot" => "Restore",
        "EnterEditMode" => "Edit",
        "ResolveConflicts" => "Conflicts",
        "UndoCommit" | "SquashCommits" => "Commit",
        _ => "Change",
    }
}

/// Absolute local time. A fake counter below year 2000 reads as just recorded.
pub fn snapshot_when(created_at_ms: i128) -> String {
    if created_at_ms < 946_684_800_000 {
        return "just recorded".to_string();
    }
    let seconds = (created_at_ms / 1000) as i64;
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown time".to_string())
}

pub fn file_change_label(status: FileStatus) -> &'static str {
    status.label()
}

impl Store {
    pub fn on_pages_project_change(&mut self) {
        self.pages.reset();
        self.sync_branch_filter();
    }

    /// True while the branches-page text box holds a filter the lists have not
    /// picked up yet. The window schedules a debounce for it.
    pub fn branch_filter_pending(&self) -> bool {
        self.derived.borrow().branch_filter != self.pages.filter
    }

    /// Point the branch and review lists at the typed filter now. Typing
    /// defers this through a debounce; tab changes and dev commands call it
    /// at once.
    pub fn commit_branch_filter(&mut self) {
        let changed = self.branch_filter_pending();
        self.derived
            .borrow_mut()
            .branch_filter
            .clone_from(&self.pages.filter);
        if changed {
            self.clamp_branch_cursor();
            self.clamp_review_cursor();
        }
    }

    /// Make the committed filter match the text box without clamping. Used
    /// when the filter is replaced wholesale (a page reset), not typed.
    pub(crate) fn sync_branch_filter(&mut self) {
        self.derived
            .borrow_mut()
            .branch_filter
            .clone_from(&self.pages.filter);
    }

    /// `Some` while a commit is checked out. Mutation controls use this as their tooltip.
    pub fn mutation_blocked(&self) -> Option<&'static str> {
        self.pages.editing().then_some(EDIT_LOCK)
    }

    pub fn note_edit_lock(&mut self) {
        self.toast_bad = true;
        self.toast = Some(EDIT_LOCK.to_string());
    }

    pub fn edit_strip_text(&self) -> String {
        let short = self
            .pages
            .edit_short
            .clone()
            .unwrap_or_else(|| "commit".to_string());
        match self.pages.phase {
            EditPhase::Entering => format!("Opening commit {short}…"),
            EditPhase::Saving => format!("Saving commit {short}…"),
            EditPhase::Aborting => format!("Aborting the edit of {short}…"),
            EditPhase::Editing => format!("Editing commit {short}"),
            EditPhase::Idle => String::new(),
        }
    }

    pub fn edit_actions_ready(&self) -> bool {
        self.pages.phase == EditPhase::Editing
    }

    /// Keys for an open branches page, history panel, or conflict panel.
    /// `None` lets the rest of the window handle the key.
    pub fn apply_page_key(&mut self, key: &str) -> Option<Effect> {
        // Command chords stay with the window, so Refresh still works over a page.
        if key.starts_with("cmd-") {
            return None;
        }
        if self.pages.ask.is_some() {
            return Some(self.page_ask_key(key));
        }
        // A context menu or a confirm owns the arrows, Enter, and Escape.
        if self.menu.is_some() || self.confirm.is_some() {
            return None;
        }
        if self.pages.branches_open {
            return Some(self.branches_key(key));
        }
        if self.pages.history_open {
            if key == "b" && !self.pages.message_focused {
                return Some(self.show_page(AppPage::Branches));
            }
            return Some(self.history_key(key));
        }
        if self.pages.conflicts_open {
            return Some(self.conflicts_key(key));
        }
        if self.typing_elsewhere() {
            return None;
        }
        match key {
            "b" => Some(self.show_page(AppPage::Branches)),
            "h" => Some(self.show_page(AppPage::History)),
            "e" => Some(self.edit_focused()),
            _ => None,
        }
    }

    pub fn app_page(&self) -> AppPage {
        if self.pages.branches_open {
            AppPage::Branches
        } else if self.pages.history_open {
            AppPage::History
        } else {
            AppPage::Workspace
        }
    }

    /// Switch the topbar page. Filter, selection, and scroll on the other pages stay.
    pub fn show_page(&mut self, page: AppPage) -> Effect {
        match page {
            AppPage::Workspace => {
                self.close_branches();
                self.close_history();
                Effect::None
            }
            AppPage::Branches => {
                if self.pages.branches_open {
                    Effect::None
                } else {
                    self.open_branches()
                }
            }
            AppPage::History => {
                if self.pages.history_open {
                    Effect::None
                } else {
                    self.open_history()
                }
            }
        }
    }

    pub fn open_branches(&mut self) -> Effect {
        self.prepare_page();
        self.pages.history_open = false;
        self.pages.conflicts_open = false;
        self.pages.branches_open = true;
        self.pages.branches_loading = true;
        self.pages.branches_error = None;
        Effect::Page(PageOp::ListBranches)
    }

    pub fn close_branches(&mut self) {
        self.pages.branches_open = false;
        self.pages.ask = None;
    }

    pub fn set_branch_tab(&mut self, tab: BranchTab) {
        self.pages.tab = tab;
        self.sync_branch_filter();
        self.clamp_branch_cursor();
        self.clamp_review_cursor();
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn set_branch_filter(&mut self, text: String) {
        self.pages.filter = text;
        self.pages.branches_open = true;
        self.sync_branch_filter();
        self.clamp_branch_cursor();
        self.clamp_review_cursor();
    }

    pub fn apply_named(&mut self, name: &str) -> Result<Effect, String> {
        self.ensure_can_write()?;
        let name = name.trim();
        if name.is_empty() {
            return Err("Choose a branch.".to_string());
        }
        if let Some(row) = self.find_listed(name)
            && row.applied
        {
            return Err(format!(
                "{} is already in the workspace. Unapply it instead.",
                row.name
            ));
        }
        let row = self.find_listed(name).cloned();
        let short = row
            .as_ref()
            .map(|row| row.name.clone())
            .unwrap_or_else(|| name.strip_prefix("refs/heads/").unwrap_or(name).to_string());
        let ref_name = row
            .map(|row| row.ref_name)
            .unwrap_or_else(|| name.to_string());
        self.pages.branch_busy = Some(short.clone());
        self.pages.branch_notice = None;
        Ok(Effect::Page(PageOp::Apply {
            name: short,
            ref_name,
        }))
    }

    pub fn unapply_named(&mut self, name: Option<&str>) -> Result<Effect, String> {
        self.ensure_can_write()?;
        let row = self.branch_for_action(name)?.clone();
        if !row.applied {
            return Err(format!("{} is not in the workspace.", row.name));
        }
        let name = row.name;
        self.pages.branch_busy = Some(name.clone());
        self.pages.branch_notice = None;
        Ok(Effect::Page(PageOp::Unapply { name }))
    }

    /// Take a workspace lane out without waiting for the branches list to load.
    pub fn unapply_workspace_lane(&mut self, name: &str) -> Result<Effect, String> {
        self.ensure_can_write()?;
        self.ensure_lane(name)?;
        let name = name.to_string();
        self.pages.branch_busy = Some(name.clone());
        self.pages.branch_notice = None;
        self.menu = None;
        Ok(Effect::Page(PageOp::Unapply { name }))
    }

    /// Open the destructive confirm. The git ref is removed only after confirm.
    pub fn ask_delete_branch(&mut self, name: Option<&str>) -> Result<(), String> {
        self.ensure_can_write()?;
        let row = self.branch_for_action(name)?.clone();
        if row.applied {
            return Err(format!(
                "Unapply {} before deleting it. It is in the workspace.",
                row.name
            ));
        }
        if row.place == BranchPlace::Remote {
            return Err("Only a local branch can be deleted.".to_string());
        }
        let name = row.name;
        self.pages.ask = Some(PageAsk {
            title: format!("Delete {name}?"),
            message: format!(
                "The local branch {name} is removed. This does not unapply a lane, and it cannot be undone from this page."
            ),
            confirm_label: "Delete branch".to_string(),
            destructive: true,
            op: PageOp::Delete { name },
        });
        Ok(())
    }

    /// Dev socket path. The click path uses [`Self::ask_delete_branch`].
    #[cfg(any(test, feature = "devtools"))]
    pub fn delete_named(&mut self, name: Option<&str>) -> Result<Effect, String> {
        self.ensure_can_write()?;
        let row = self.branch_for_action(name)?.clone();
        if row.applied {
            return Err(format!(
                "Unapply {} before deleting it. It is in the workspace.",
                row.name
            ));
        }
        if row.place == BranchPlace::Remote {
            return Err("Only a local branch can be deleted.".to_string());
        }
        let name = row.name;
        self.pages.ask = None;
        self.pages.branch_busy = Some(name.clone());
        self.pages.branch_notice = None;
        Ok(Effect::Page(PageOp::Delete { name }))
    }

    pub fn open_history(&mut self) -> Effect {
        self.prepare_page();
        self.pages.branches_open = false;
        self.pages.conflicts_open = false;
        self.pages.history_open = true;
        self.pages.history_loading = true;
        self.pages.history_error = None;
        Effect::Page(PageOp::ListSnapshots)
    }

    pub fn close_history(&mut self) {
        self.pages.history_open = false;
        self.pages.message_focused = false;
        self.pages.ask = None;
    }

    pub fn select_snapshot_id(&mut self, id: &str) -> Result<Effect, String> {
        self.ensure_project()?;
        if self.pages.snapshots.is_empty() {
            self.pages.history_open = true;
            self.pages.history_loading = true;
            self.pages.snapshot_sha = Some(id.to_string());
            return Ok(Effect::Page(PageOp::ListSnapshots));
        }
        let Some(index) = find_snapshot(&self.pages.snapshots, id) else {
            return Err(format!("No snapshot {id}"));
        };
        self.select_snapshot_at(index)
    }

    #[cfg(feature = "devtools")]
    pub fn set_snapshot_message(&mut self, message: String) {
        self.pages.snapshot_message = message;
        self.pages.message_focused = true;
        self.pages.history_open = true;
    }

    pub fn create_snapshot_now(&mut self, message: &str) -> Result<Effect, String> {
        self.ensure_can_write()?;
        self.pages.history_open = true;
        self.pages.history_busy = true;
        self.pages.history_notice = None;
        self.pages.message_focused = false;
        Ok(Effect::Page(PageOp::CreateSnapshot {
            message: message.to_string(),
        }))
    }

    pub fn ask_restore(&mut self) -> Result<(), String> {
        self.ensure_can_write()?;
        let snapshot = self
            .pages
            .selected_snapshot()
            .cloned()
            .ok_or_else(|| "Select a snapshot first.".to_string())?;
        if !snapshot.restorable {
            return Err("Undo and redo entries are not restored from here. Use Undo.".to_string());
        }
        let label = snapshot.title.clone();
        self.pages.ask = Some(PageAsk {
            title: "Restore to here?".to_string(),
            message: format!(
                "The project returns to \"{label}\". Undo can bring the current state back."
            ),
            confirm_label: "Restore to here".to_string(),
            destructive: true,
            op: PageOp::Restore { sha: snapshot.sha },
        });
        Ok(())
    }

    /// Dev socket path. The button asks first.
    #[cfg(any(test, feature = "devtools"))]
    pub fn restore_selected(&mut self) -> Result<Effect, String> {
        self.ensure_can_write()?;
        let snapshot = self
            .pages
            .selected_snapshot()
            .cloned()
            .ok_or_else(|| "Select a snapshot first.".to_string())?;
        if !snapshot.restorable {
            return Err("Undo and redo entries are not restored from here. Use Undo.".to_string());
        }
        self.pages.ask = None;
        self.pages.history_busy = true;
        self.pages.history_notice = None;
        Ok(Effect::Page(PageOp::Restore { sha: snapshot.sha }))
    }

    pub fn request_enter_edit(&mut self, sha: &str) -> Result<Effect, String> {
        self.ensure_project()?;
        if self.pages.editing() {
            return Err(EDIT_LOCK.to_string());
        }
        let sha = self.resolve_sha(sha);
        if !self.workspace_has_commit(&sha) {
            return Err(format!("No commit {sha}"));
        }
        self.pages.phase = step_edit(self.pages.phase, EditEvent::Enter).map_err(str::to_string)?;
        self.pages.edit_sha = Some(sha.clone());
        self.pages.edit_short = Some(short7(&sha));
        self.pages.edit_error = None;
        self.menu = None;
        self.confirm = None;
        self.pages.ask = None;
        Ok(Effect::Page(PageOp::Enter { sha }))
    }

    pub fn request_save_edit(&mut self) -> Result<Effect, String> {
        self.begin_edit_exit(EditEvent::Save, PageOp::SaveEdit)
    }

    pub fn request_abort_edit(&mut self) -> Result<Effect, String> {
        self.begin_edit_exit(EditEvent::Abort, PageOp::AbortEdit)
    }

    pub fn edit_focused(&mut self) -> Effect {
        let Some(sha) = self.focused_commit_sha() else {
            self.toast_bad = true;
            self.toast = Some("Focus a commit to edit it.".to_string());
            return Effect::None;
        };
        match self.request_enter_edit(&sha) {
            Ok(effect) => effect,
            Err(error) => {
                self.toast_bad = true;
                self.toast = Some(error);
                Effect::None
            }
        }
    }

    pub fn open_conflicts(&mut self, sha: Option<&str>) -> Result<Effect, String> {
        self.ensure_project()?;
        if self.pages.editing() {
            return Err(EDIT_LOCK.to_string());
        }
        let sha = self.conflict_target(sha)?;
        if self.pages.conflicts_open
            && self.pages.conflict_sha.as_deref() == Some(sha.as_str())
            && !self.pages.conflict_files.is_empty()
        {
            return Ok(Effect::None);
        }
        self.prepare_page();
        self.pages.branches_open = false;
        self.pages.history_open = false;
        self.pages.conflicts_open = true;
        self.pages.conflict_sha = Some(sha.clone());
        self.pages.conflict_title.clear();
        self.pages.conflict_files.clear();
        self.pages.conflict_cursor = 0;
        self.pages.conflict_notice = None;
        self.pages.conflict_busy = true;
        Ok(Effect::Page(PageOp::ConflictFiles { sha }))
    }

    pub fn close_conflicts(&mut self) {
        self.pages.conflicts_open = false;
        self.pages.conflict_busy = false;
    }

    pub fn choose_side(&mut self, path: &str, side: &str) -> Result<(), String> {
        if !self.pages.conflicts_open {
            return Err("Open Resolve conflicts first.".to_string());
        }
        let choice = parse_side(side)?;
        set_conflict_side(&mut self.pages.conflict_files, path, choice)?;
        if let Some(index) = self
            .pages
            .conflict_files
            .iter()
            .position(|file| file.path == path)
        {
            self.pages.conflict_cursor = index;
        }
        self.pages.conflict_notice = None;
        Ok(())
    }

    pub fn mark_resolved(&mut self) -> Result<Effect, String> {
        self.ensure_can_write()?;
        let sha = self
            .pages
            .conflict_sha
            .clone()
            .ok_or_else(|| "Open Resolve conflicts first.".to_string())?;
        let files = resolution_plan(&self.pages.conflict_files).map_err(str::to_string)?;
        self.pages.conflict_busy = true;
        self.pages.conflict_notice = None;
        Ok(Effect::Page(PageOp::Resolve { sha, files }))
    }

    pub fn confirm_page_yes(&mut self) -> Effect {
        let Some(ask) = self.pages.ask.take() else {
            return Effect::None;
        };
        if self.mutation_blocked().is_some() {
            self.note_edit_lock();
            return Effect::None;
        }
        match &ask.op {
            PageOp::Delete { name } => self.pages.branch_busy = Some(name.clone()),
            PageOp::Restore { .. } => self.pages.history_busy = true,
            _ => {}
        }
        Effect::Page(ask.op)
    }

    pub fn confirm_page_no(&mut self) {
        self.pages.ask = None;
    }

    pub fn accept_page(&mut self, done: PageDone) -> Effect {
        let id = page_done_id(&done);
        let message = self.activity.done_text(id).to_string();
        self.activity.succeed(id, message);
        match done {
            PageDone::Branches(rows) => {
                let keep = self
                    .pages
                    .selected_branch()
                    .map(|row| row.name.clone())
                    .or_else(|| self.pages.branch_busy.clone());
                self.set_branch_rows(rows);
                self.pages.branches_loading = false;
                self.pages.branches_error = None;
                self.pages.branch_busy = None;
                if let Some(name) = keep {
                    self.place_branch(&name);
                } else {
                    self.clamp_branch_cursor();
                }
                Effect::None
            }
            PageDone::Applied {
                name,
                workspace,
                worktree,
                already,
            } => {
                self.take_workspace(workspace, worktree);
                self.pages.branch_busy = None;
                let text = if already {
                    format!("{name} is already in the workspace.")
                } else {
                    format!("Applied {name} to the workspace.")
                };
                self.note_branches(text, false);
                self.follow_branches()
            }
            PageDone::Unapplied {
                name,
                workspace,
                worktree,
            } => {
                self.take_workspace(workspace, worktree);
                self.pages.branch_busy = None;
                self.note_branches(
                    format!("Unapplied {name}. The git branch is still there."),
                    false,
                );
                self.follow_branches()
            }
            PageDone::Deleted {
                name,
                workspace,
                worktree,
            } => {
                self.take_workspace(workspace, worktree);
                self.pages.branch_busy = None;
                self.note_branches(format!("Deleted {name}."), false);
                self.follow_branches()
            }
            PageDone::Snapshots(list) => {
                let pending = self.pages.snapshot_sha.clone();
                self.pages.snapshots = list;
                self.pages.history_loading = false;
                self.pages.history_error = None;
                self.pages.history_busy = false;
                if let Some(id) = pending
                    && let Some(index) = find_snapshot(&self.pages.snapshots, &id)
                {
                    return self.select_snapshot_at(index).unwrap_or(Effect::None);
                }
                self.clamp_snapshot_cursor();
                if self.pages.snapshot_sha.is_none() {
                    return self.load_selected_diff();
                }
                Effect::None
            }
            PageDone::Diff { sha, files } => {
                if self.pages.snapshot_sha.as_deref() == Some(sha.as_str()) {
                    self.pages.snapshot_files = Arc::new(files);
                    self.pages.diff_loading = false;
                }
                Effect::None
            }
            PageDone::Created(info) => {
                let title = info.title.clone();
                let sha = info.sha.clone();
                if self
                    .pages
                    .snapshots
                    .iter()
                    .all(|snapshot| snapshot.sha != sha)
                {
                    self.pages.snapshots.insert(0, info);
                }
                self.pages.snapshot_cursor = 0;
                self.pages.snapshot_sha = Some(sha.clone());
                self.pages.snapshot_message.clear();
                self.pages.message_focused = false;
                self.pages.history_busy = false;
                self.pages.history_open = true;
                self.pages.diff_loading = true;
                self.pages.snapshot_files = Arc::new(Vec::new());
                self.note_history(format!("Saved snapshot {title}."), false);
                Effect::Page(PageOp::SnapshotDiff { sha })
            }
            PageDone::Restored {
                workspace,
                worktree,
                sha,
            } => {
                self.take_workspace(workspace, worktree);
                self.pages.history_busy = false;
                self.pages.snapshot_sha = Some(sha);
                self.note_history("Restored the snapshot.".to_string(), false);
                self.pages.history_loading = true;
                Effect::Page(PageOp::ListSnapshots)
            }
            PageDone::Editing(status) => self.accept_edit_status(status),
            PageDone::Saved {
                workspace,
                worktree,
            } => {
                self.take_workspace(workspace, worktree);
                self.finish_edit("Saved the edit and returned to the workspace.");
                Effect::None
            }
            PageDone::Aborted {
                workspace,
                worktree,
            } => {
                self.take_workspace(workspace, worktree);
                self.finish_edit("Aborted the edit. The commit is unchanged.");
                Effect::None
            }
            PageDone::ConflictFiles { sha, title, paths } => {
                if self.pages.conflict_sha.as_deref() != Some(sha.as_str()) {
                    return Effect::None;
                }
                self.pages.conflict_title = title;
                self.pages.conflict_files = paths
                    .into_iter()
                    .map(|path| ConflictFile { path, side: None })
                    .collect();
                self.pages.conflict_cursor = 0;
                self.pages.conflict_busy = false;
                if self.pages.conflict_files.is_empty() {
                    self.note_conflicts(
                        "This commit has no files to resolve. Finish it in edit mode.".to_string(),
                        true,
                    );
                }
                Effect::None
            }
            PageDone::Resolved {
                workspace,
                worktree,
                previous: _,
                new_sha,
            } => {
                self.take_workspace(workspace, worktree);
                self.pages.conflicts_open = false;
                self.pages.conflict_busy = false;
                self.pages.conflict_files.clear();
                let short = short7(&new_sha);
                self.toast_bad = false;
                self.toast = Some(format!("Resolved conflicts. The commit is now {short}."));
                if self.commit_still_focused() {
                    return self.focus_commit(&new_sha);
                }
                Effect::None
            }
        }
    }

    pub fn fail_page(&mut self, op: &PageOp, message: String) {
        let (id, _) = page_activity(op);
        self.activity.fail(id, message.clone());
        match op {
            PageOp::ListBranches => {
                self.pages.branches_loading = false;
                self.pages.branches_error = Some(message.clone());
            }
            PageOp::Apply { .. } | PageOp::Unapply { .. } | PageOp::Delete { .. } => {
                self.pages.branch_busy = None;
                self.note_branches(message.clone(), true);
            }
            PageOp::ListSnapshots => {
                self.pages.history_loading = false;
                self.pages.history_busy = false;
                self.pages.history_error = Some(message.clone());
            }
            PageOp::SnapshotDiff { .. } => {
                self.pages.diff_loading = false;
                self.note_history(message.clone(), true);
            }
            PageOp::CreateSnapshot { .. } | PageOp::Restore { .. } => {
                self.pages.history_busy = false;
                self.note_history(message.clone(), true);
            }
            PageOp::Enter { .. } => {
                self.pages.phase =
                    step_edit(self.pages.phase, EditEvent::Failed).unwrap_or(EditPhase::Idle);
                if self.pages.phase == EditPhase::Idle {
                    self.pages.edit_sha = None;
                    self.pages.edit_short = None;
                }
                self.pages.edit_error = Some(message.clone());
            }
            PageOp::SaveEdit | PageOp::AbortEdit => {
                self.pages.phase =
                    step_edit(self.pages.phase, EditEvent::Failed).unwrap_or(EditPhase::Editing);
                self.pages.edit_error = Some(message.clone());
            }
            PageOp::ConflictFiles { .. } | PageOp::Resolve { .. } => {
                self.pages.conflict_busy = false;
                self.note_conflicts(message.clone(), true);
            }
        }
        self.toast_bad = true;
        self.toast = Some(message);
    }
}

impl Store {
    fn prepare_page(&mut self) {
        self.project_menu = false;
        self.help = false;
        self.menu = None;
        self.confirm = None;
        self.pages.ask = None;
        self.remote.close_overlays();
    }

    fn typing_elsewhere(&self) -> bool {
        self.composer.field.is_some()
            || self.reword.is_some()
            || self.menu.as_ref().is_some_and(|menu| menu.naming)
            || self.remote.askpass.is_some()
            || self.remote.push_remote_focused
            || self.remote.add_focused
    }

    fn ensure_project(&self) -> Result<(), String> {
        if self.selected.is_none() {
            Err("Choose a project first.".to_string())
        } else {
            Ok(())
        }
    }

    fn ensure_can_write(&mut self) -> Result<(), String> {
        self.ensure_project()?;
        if self.pages.editing() {
            self.note_edit_lock();
            return Err(EDIT_LOCK.to_string());
        }
        Ok(())
    }

    fn page_ask_key(&mut self, key: &str) -> Effect {
        match key {
            "enter" | "y" => self.confirm_page_yes(),
            "escape" | "n" => {
                self.confirm_page_no();
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn branches_key(&mut self, key: &str) -> Effect {
        if key.starts_with("cmd-") {
            return Effect::None;
        }
        match key {
            "escape" => {
                self.close_branches();
                Effect::None
            }
            "up" => {
                self.move_branch_or_review(-1);
                Effect::None
            }
            "down" => {
                self.move_branch_or_review(1);
                Effect::None
            }
            "tab" => {
                self.set_branch_tab(self.pages.tab.next(1));
                Effect::None
            }
            "shift-tab" => {
                self.set_branch_tab(self.pages.tab.next(-1));
                Effect::None
            }
            "enter" => self.primary_branch_action(),
            "backspace" => {
                self.pages.filter.pop();
                self.clamp_branch_cursor();
                self.clamp_review_cursor();
                Effect::None
            }
            other if is_typed(other) => {
                self.pages.filter.push_str(other);
                self.clamp_branch_cursor();
                self.clamp_review_cursor();
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn history_key(&mut self, key: &str) -> Effect {
        if self.pages.message_focused && !key.starts_with("cmd-") {
            return self.message_key(key);
        }
        if key.starts_with("cmd-") {
            return Effect::None;
        }
        match key {
            "escape" => {
                self.close_history();
                Effect::None
            }
            "up" => self.move_snapshot(-1),
            "down" => self.move_snapshot(1),
            "enter" => match self.ask_restore() {
                Ok(()) => Effect::None,
                Err(error) => {
                    self.note_history(error.clone(), true);
                    self.toast_bad = true;
                    self.toast = Some(error);
                    Effect::None
                }
            },
            "n" => {
                self.pages.message_focused = true;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn message_key(&mut self, key: &str) -> Effect {
        match key {
            "escape" => {
                self.pages.message_focused = false;
                Effect::None
            }
            "enter" => {
                let message = self.pages.snapshot_message.clone();
                match self.create_snapshot_now(&message) {
                    Ok(effect) => effect,
                    Err(error) => {
                        self.note_history(error.clone(), true);
                        self.toast_bad = true;
                        self.toast = Some(error);
                        Effect::None
                    }
                }
            }
            "backspace" => {
                self.pages.snapshot_message.pop();
                Effect::None
            }
            other if is_typed(other) => {
                self.pages.snapshot_message.push_str(other);
                Effect::None
            }
            "up" | "down" => {
                self.pages.message_focused = false;
                if key == "up" {
                    self.move_snapshot(-1)
                } else {
                    self.move_snapshot(1)
                }
            }
            _ => Effect::None,
        }
    }

    fn conflicts_key(&mut self, key: &str) -> Effect {
        if key.starts_with("cmd-") {
            return Effect::None;
        }
        match key {
            "escape" => {
                self.close_conflicts();
                Effect::None
            }
            "up" => {
                self.move_conflict(-1);
                Effect::None
            }
            "down" => {
                self.move_conflict(1);
                Effect::None
            }
            "left" | "o" => self.pick_focused_side(ConflictChoice::Ours),
            "right" | "t" => self.pick_focused_side(ConflictChoice::Theirs),
            "enter" => match self.mark_resolved() {
                Ok(effect) => effect,
                Err(error) => {
                    self.note_conflicts(error.clone(), true);
                    self.toast_bad = true;
                    self.toast = Some(error);
                    Effect::None
                }
            },
            _ => Effect::None,
        }
    }

    fn pick_focused_side(&mut self, side: ConflictChoice) -> Effect {
        let Some(path) = self.pages.conflict_file().map(|file| file.path.clone()) else {
            return Effect::None;
        };
        let _ = set_conflict_side(&mut self.pages.conflict_files, &path, side);
        self.pages.conflict_notice = None;
        Effect::None
    }

    fn move_branch_or_review(&mut self, step: i32) {
        if self.pages.tab == BranchTab::PullRequests {
            self.move_review(step);
        } else {
            self.move_branch(step);
        }
    }

    fn primary_branch_action(&mut self) -> Effect {
        if self.pages.tab == BranchTab::PullRequests {
            return Effect::None;
        }
        let Some(row) = self.pages.selected_branch().cloned() else {
            return Effect::None;
        };
        let result = if row.applied {
            self.unapply_named(Some(&row.name))
        } else {
            self.apply_named(&row.name)
        };
        match result {
            Ok(effect) => effect,
            Err(error) => {
                self.note_branches(error.clone(), true);
                self.toast_bad = true;
                self.toast = Some(error);
                Effect::None
            }
        }
    }

    fn move_branch(&mut self, step: i32) {
        let len = self.pages.visible_branches().len();
        self.pages.branch_cursor = move_index(len, self.pages.branch_cursor, step);
    }

    fn clamp_branch_cursor(&mut self) {
        let len = self.pages.visible_branches().len();
        if len == 0 {
            self.pages.branch_cursor = 0;
        } else if self.pages.branch_cursor >= len {
            self.pages.branch_cursor = len - 1;
        }
    }

    pub fn place_branch(&mut self, name: &str) {
        let visible = self.pages.visible_branches();
        if let Some(index) = visible.iter().position(|row| row.name == name) {
            self.pages.branch_cursor = index;
        } else {
            self.clamp_branch_cursor();
        }
    }

    pub(crate) fn find_listed(&self, name: &str) -> Option<&BranchRow> {
        let wanted = name.trim();
        self.pages.branch_rows.iter().find(|row| {
            row.name == wanted
                || row.ref_name == wanted
                || row.tip_short == wanted
                || format!("refs/heads/{}", row.name) == wanted
        })
    }

    fn branch_for_action(&self, name: Option<&str>) -> Result<&BranchRow, String> {
        if let Some(name) = name {
            return self
                .find_listed(name)
                .ok_or_else(|| format!("No branch {name}"));
        }
        self.pages
            .selected_branch()
            .ok_or_else(|| "Select a branch first.".to_string())
    }

    fn move_snapshot(&mut self, step: i32) -> Effect {
        let len = self.pages.snapshots.len();
        let next = move_index(len, self.pages.snapshot_cursor, step);
        self.select_snapshot_at(next).unwrap_or(Effect::None)
    }

    fn select_snapshot_at(&mut self, index: usize) -> Result<Effect, String> {
        let Some(snapshot) = self.pages.snapshots.get(index) else {
            return Err("Select a snapshot first.".to_string());
        };
        let sha = snapshot.sha.clone();
        self.pages.snapshot_cursor = index;
        self.pages.snapshot_sha = Some(sha.clone());
        self.pages.snapshot_files = Arc::new(Vec::new());
        self.pages.diff_loading = true;
        self.pages.history_error = None;
        Ok(Effect::Page(PageOp::SnapshotDiff { sha }))
    }

    fn clamp_snapshot_cursor(&mut self) {
        let len = self.pages.snapshots.len();
        if len == 0 {
            self.pages.snapshot_cursor = 0;
            return;
        }
        if self.pages.snapshot_cursor >= len {
            self.pages.snapshot_cursor = len - 1;
        }
    }

    fn load_selected_diff(&mut self) -> Effect {
        let Some(snapshot) = self.pages.selected_snapshot() else {
            return Effect::None;
        };
        let sha = snapshot.sha.clone();
        self.pages.snapshot_sha = Some(sha.clone());
        self.pages.diff_loading = true;
        self.pages.snapshot_files = Arc::new(Vec::new());
        Effect::Page(PageOp::SnapshotDiff { sha })
    }

    fn move_conflict(&mut self, step: i32) {
        let len = self.pages.conflict_files.len();
        self.pages.conflict_cursor = move_index(len, self.pages.conflict_cursor, step);
    }

    fn begin_edit_exit(&mut self, event: EditEvent, op: PageOp) -> Result<Effect, String> {
        self.ensure_project()?;
        self.pages.phase = step_edit(self.pages.phase, event).map_err(str::to_string)?;
        self.pages.edit_error = None;
        Ok(Effect::Page(op))
    }

    fn accept_edit_status(&mut self, status: EditStatus) -> Effect {
        if !status.editing {
            self.pages.phase =
                step_edit(self.pages.phase, EditEvent::Failed).unwrap_or(EditPhase::Idle);
            self.pages.edit_sha = None;
            self.pages.edit_short = None;
            self.pages.edit_error = Some("GitButler did not enter edit mode.".to_string());
            self.toast_bad = true;
            self.toast = self.pages.edit_error.clone();
            return Effect::None;
        }
        self.pages.phase =
            step_edit(self.pages.phase, EditEvent::Entered).unwrap_or(EditPhase::Editing);
        if let Some(sha) = status.commit_sha {
            self.pages.edit_short = Some(short7(&sha));
            self.pages.edit_sha = Some(sha);
        }
        self.pages.edit_error = None;
        Effect::None
    }

    fn finish_edit(&mut self, message: &str) {
        let _ = step_edit(self.pages.phase, EditEvent::Finished);
        self.pages.phase = EditPhase::Idle;
        self.pages.edit_sha = None;
        self.pages.edit_short = None;
        self.pages.edit_error = None;
        self.toast_bad = false;
        self.toast = Some(message.to_string());
    }

    fn take_workspace(&mut self, workspace: WorkspaceSummary, worktree: WorktreeView) {
        self.bump_data();
        self.workspace = Some(Arc::new(workspace));
        self.worktree = Some(Arc::new(worktree));
        self.clamp();
        if let ShownFile::Worktree { path } = &self.shown
            && self
                .worktree
                .as_ref()
                .is_none_or(|worktree| !worktree.files.iter().any(|file| file.path == *path))
        {
            self.shown = ShownFile::None;
        }
    }

    fn follow_branches(&mut self) -> Effect {
        if self.pages.branches_open {
            self.pages.branches_loading = true;
            Effect::Page(PageOp::ListBranches)
        } else {
            Effect::None
        }
    }

    fn note_branches(&mut self, text: String, bad: bool) {
        self.pages.branch_notice = Some(text.clone());
        self.pages.branch_notice_bad = bad;
        self.note_panel(text, bad, self.pages.branches_open);
    }

    fn note_history(&mut self, text: String, bad: bool) {
        self.pages.history_notice = Some(text.clone());
        self.pages.history_notice_bad = bad;
        self.note_panel(text, bad, self.pages.history_open);
    }

    fn note_conflicts(&mut self, text: String, bad: bool) {
        self.pages.conflict_notice = Some(text.clone());
        self.pages.conflict_notice_bad = bad;
        self.note_panel(text, bad, self.pages.conflicts_open);
    }

    /// The open panel already shows the sentence. A toast is for a closed panel or an error.
    fn note_panel(&mut self, text: String, bad: bool, panel_open: bool) {
        if bad || !panel_open {
            self.toast_bad = bad;
            self.toast = Some(text);
        } else if self.toast_bad {
            self.toast = None;
            self.toast_bad = false;
        }
    }

    fn focused_commit_sha(&self) -> Option<String> {
        match &self.side {
            SideFocus::Commit { sha } => Some(self.resolve_sha(sha)),
            _ => None,
        }
    }

    fn workspace_has_commit(&self, sha: &str) -> bool {
        self.workspace.as_ref().is_some_and(|workspace| {
            workspace.stacks.iter().any(|stack| {
                stack.branches.iter().any(|branch| {
                    branch
                        .commits
                        .iter()
                        .any(|commit| commit.sha == sha || commit.short_sha == sha)
                })
            })
        })
    }

    pub(crate) fn commit_is_conflicted(&self, sha: &str) -> bool {
        self.workspace.as_ref().is_some_and(|workspace| {
            workspace.stacks.iter().any(|stack| {
                stack.branches.iter().any(|branch| {
                    branch.commits.iter().any(|commit| {
                        (commit.sha == sha || commit.short_sha == sha) && commit.conflicted
                    })
                })
            })
        })
    }

    fn commit_still_focused(&self) -> bool {
        matches!(self.side, SideFocus::Commit { .. })
    }

    fn conflict_target(&self, sha: Option<&str>) -> Result<String, String> {
        if let Some(sha) = sha {
            let full = self.resolve_sha(sha);
            if !self.commit_is_conflicted(&full) {
                return Err("That commit has no conflicts.".to_string());
            }
            return Ok(full);
        }
        if let Some(sha) = self.focused_commit_sha()
            && self.commit_is_conflicted(&sha)
        {
            return Ok(sha);
        }
        self.workspace
            .as_ref()
            .and_then(|workspace| {
                workspace.stacks.iter().find_map(|stack| {
                    stack.branches.iter().find_map(|branch| {
                        branch
                            .commits
                            .iter()
                            .find(|commit| commit.conflicted)
                            .map(|commit| commit.sha.clone())
                    })
                })
            })
            .ok_or_else(|| "No conflicted commit.".to_string())
    }
}

pub fn perform_page(
    backend: &dyn Backend,
    project: Option<&str>,
    op: &PageOp,
) -> Result<PageDone, String> {
    let id = project.ok_or_else(|| "Choose a project first.".to_string())?;
    match op {
        PageOp::ListBranches => backend
            .list_branches(id)
            .map(PageDone::Branches)
            .map_err(bridge_message),
        PageOp::Apply { name, ref_name } => {
            let applied = backend.apply_branch(id, ref_name).map_err(bridge_message)?;
            let worktree = read_worktree(backend, id)?;
            Ok(PageDone::Applied {
                name: name.clone(),
                workspace: applied.workspace,
                worktree,
                already: applied.already_applied,
            })
        }
        PageOp::Unapply { name } => {
            let workspace = backend.unapply_branch(id, name).map_err(bridge_message)?;
            let worktree = read_worktree(backend, id)?;
            Ok(PageDone::Unapplied {
                name: name.clone(),
                workspace,
                worktree,
            })
        }
        PageOp::Delete { name } => {
            let workspace = backend
                .delete_local_branch(id, name)
                .map_err(bridge_message)?;
            let worktree = read_worktree(backend, id)?;
            Ok(PageDone::Deleted {
                name: name.clone(),
                workspace,
                worktree,
            })
        }
        PageOp::ListSnapshots => backend
            .list_snapshots(id, SNAPSHOT_LIMIT)
            .map(PageDone::Snapshots)
            .map_err(bridge_message),
        PageOp::SnapshotDiff { sha } => backend
            .snapshot_diff(id, sha)
            .map(|files| PageDone::Diff {
                sha: sha.clone(),
                files,
            })
            .map_err(bridge_message),
        PageOp::CreateSnapshot { message } => backend
            .create_snapshot(id, message)
            .map(PageDone::Created)
            .map_err(bridge_message),
        PageOp::Restore { sha } => {
            let restored = backend.restore_snapshot(id, sha).map_err(bridge_message)?;
            let worktree = read_worktree(backend, id)?;
            Ok(PageDone::Restored {
                workspace: restored.workspace,
                worktree,
                sha: sha.clone(),
            })
        }
        PageOp::Enter { sha } => backend
            .enter_edit_mode(id, sha)
            .map(PageDone::Editing)
            .map_err(bridge_message),
        PageOp::SaveEdit => {
            let workspace = backend
                .save_edit_and_return_to_workspace(id)
                .map_err(bridge_message)?;
            let worktree = read_worktree(backend, id)?;
            Ok(PageDone::Saved {
                workspace,
                worktree,
            })
        }
        PageOp::AbortEdit => {
            let workspace = backend
                .abort_edit_and_return_to_workspace(id, false)
                .map_err(bridge_message)?;
            let worktree = read_worktree(backend, id)?;
            Ok(PageDone::Aborted {
                workspace,
                worktree,
            })
        }
        PageOp::ConflictFiles { sha } => {
            let details = backend.commit_details(id, sha).map_err(bridge_message)?;
            let title = details
                .message
                .lines()
                .next()
                .unwrap_or("Conflicted commit")
                .to_string();
            let paths = details.files.into_iter().map(|file| file.path).collect();
            Ok(PageDone::ConflictFiles {
                sha: sha.clone(),
                title,
                paths,
            })
        }
        PageOp::Resolve { sha, files } => {
            let report = backend
                .resolve_commit_conflict_hunks(id, sha, files)
                .map_err(bridge_message)?;
            let worktree = read_worktree(backend, id)?;
            Ok(PageDone::Resolved {
                workspace: report.workspace,
                worktree,
                previous: report.previous_sha,
                new_sha: report.new_sha,
            })
        }
    }
}

fn read_worktree(backend: &dyn Backend, id: &str) -> Result<WorktreeView, String> {
    backend.worktree(id).map_err(bridge_message)
}

fn bridge_message(error: gb_bridge::BridgeError) -> String {
    error.message
}

fn short7(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn is_typed(key: &str) -> bool {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => !ch.is_control() && ch != '\u{7f}',
        _ => false,
    }
}

pub(super) fn move_index(len: usize, cursor: usize, step: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let next = cursor as i32 + step;
    if next < 0 {
        0
    } else if next as usize >= len {
        len - 1
    } else {
        next as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, load};
    use gb_bridge::FakeBackend;

    fn row(name: &str, place: BranchPlace, applied: bool, last: &str, author: &str) -> BranchRow {
        BranchRow {
            name: name.to_string(),
            ref_name: if place == BranchPlace::Remote {
                format!("refs/remotes/origin/{name}")
            } else {
                format!("refs/heads/{name}")
            },
            place,
            remotes: if place == BranchPlace::Local {
                Vec::new()
            } else {
                vec![format!("refs/remotes/origin/{name}")]
            },
            applied,
            stack: if applied { "applied" } else { "unapplied" }.to_string(),
            stack_branches: vec![name.to_string()],
            tip_sha: format!("{name}-tip"),
            tip_short: name.chars().take(7).collect(),
            last_commit: last.to_string(),
            author: author.to_string(),
            updated_at_ms: None,
            ahead: Some(1),
            behind: Some(0),
        }
    }

    fn sample() -> Vec<BranchRow> {
        vec![
            row(
                "lane-a",
                BranchPlace::Local,
                true,
                "Wire the lane header",
                "Ada Lovelace <ada@example.com>",
            ),
            row(
                "topic",
                BranchPlace::Both,
                false,
                "Start the topic",
                "Ada Lovelace <ada@example.com>",
            ),
            row(
                "remote-only",
                BranchPlace::Remote,
                false,
                "Only on the remote",
                "Grace Hopper <grace@example.com>",
            ),
        ]
    }

    fn ready() -> (FakeBackend, Store) {
        let backend = FakeBackend::demo();
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        (backend, store)
    }

    macro_rules! drain {
        ($backend:expr, $store:ident, $effect:expr) => {{
            let effect = $effect;
            drain_effect($backend, &mut $store, effect);
        }};
    }

    fn drain_effect(backend: &FakeBackend, store: &mut Store, mut effect: Effect) {
        for _ in 0..6 {
            let Effect::Page(op) = effect else {
                return;
            };
            effect = match perform_page(backend, store.selected.as_deref(), &op) {
                Ok(done) => store.accept_page(done),
                Err(error) => {
                    store.fail_page(&op, error);
                    Effect::None
                }
            };
        }
    }

    #[test]
    fn branch_filter_keeps_the_tab_and_the_query() {
        let rows = sample();
        let local = visible_indices(&rows, BranchTab::Local, "");
        assert_eq!(local, vec![0, 1]);
        let remote = visible_indices(&rows, BranchTab::Remote, "");
        assert_eq!(remote, vec![1, 2]);
        assert!(visible_indices(&rows, BranchTab::PullRequests, "").is_empty());
        assert_eq!(visible_indices(&rows, BranchTab::Local, "TOPIC"), vec![1]);
        assert_eq!(visible_indices(&rows, BranchTab::Local, "wire"), vec![0]);
        assert_eq!(visible_indices(&rows, BranchTab::Remote, "grace"), vec![2]);
        assert!(visible_indices(&rows, BranchTab::Local, "missing").is_empty());
    }

    #[test]
    fn branch_cursor_follows_the_filtered_row() {
        let pages = Pages {
            branch_rows: sample(),
            tab: BranchTab::Local,
            filter: "topic".to_string(),
            branch_cursor: 4,
            ..Pages::default()
        };
        let visible = pages.visible_branches();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name, "topic");
        let mut store = Store {
            pages,
            ..Store::default()
        };
        store.clamp_branch_cursor();
        assert_eq!(store.pages.branch_cursor, 0);
        assert_eq!(store.pages.selected_branch().unwrap().name, "topic");
    }

    #[test]
    fn snapshot_selection_accepts_sha_prefix_or_index() {
        let snapshots = vec![
            SnapshotInfo {
                sha: "abc1234dead".to_string(),
                created_at_ms: 1_700_000_000_000,
                operation: "OnDemandSnapshot".to_string(),
                title: "Checkpoint".to_string(),
                restorable: true,
            },
            SnapshotInfo {
                sha: "fff0000beef".to_string(),
                created_at_ms: 10,
                operation: "RestoreFromSnapshotViaUndo".to_string(),
                title: "Undo".to_string(),
                restorable: false,
            },
        ];
        assert_eq!(find_snapshot(&snapshots, "abc1234dead"), Some(0));
        assert_eq!(find_snapshot(&snapshots, "abc1234"), Some(0));
        assert_eq!(find_snapshot(&snapshots, "fff0000"), Some(1));
        assert_eq!(find_snapshot(&snapshots, "nope"), None);
        assert_eq!(snapshot_kind("OnDemandSnapshot"), "Snapshot");
        assert_eq!(snapshot_when(10), "just recorded");
        assert!(snapshot_when(1_700_000_000_000).starts_with("20"));
    }

    #[test]
    fn edit_mode_refuses_to_skip_a_step() {
        assert_eq!(
            step_edit(EditPhase::Idle, EditEvent::Enter).unwrap(),
            EditPhase::Entering
        );
        assert_eq!(
            step_edit(EditPhase::Entering, EditEvent::Entered).unwrap(),
            EditPhase::Editing
        );
        assert_eq!(
            step_edit(EditPhase::Editing, EditEvent::Save).unwrap(),
            EditPhase::Saving
        );
        assert_eq!(
            step_edit(EditPhase::Saving, EditEvent::Finished).unwrap(),
            EditPhase::Idle
        );
        assert_eq!(
            step_edit(EditPhase::Editing, EditEvent::Abort).unwrap(),
            EditPhase::Aborting
        );
        assert_eq!(
            step_edit(EditPhase::Aborting, EditEvent::Failed).unwrap(),
            EditPhase::Editing
        );
        assert!(step_edit(EditPhase::Idle, EditEvent::Save).is_err());
        assert!(step_edit(EditPhase::Editing, EditEvent::Finished).is_err());
        assert!(step_edit(EditPhase::Entering, EditEvent::Abort).is_err());
    }

    #[test]
    fn conflict_plan_needs_a_side_for_every_file() {
        let mut files = vec![
            ConflictFile {
                path: "src/helper.rs".to_string(),
                side: None,
            },
            ConflictFile {
                path: "src/other.rs".to_string(),
                side: Some(ConflictChoice::Theirs),
            },
        ];
        assert_eq!(
            resolution_plan(&files),
            Err("Choose ours or theirs for every file.")
        );
        set_conflict_side(&mut files, "src/helper.rs", ConflictChoice::Ours).unwrap();
        let plan = resolution_plan(&files).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].choice, ConflictChoice::Ours);
        assert_eq!(plan[1].choice, ConflictChoice::Theirs);
        assert!(set_conflict_side(&mut files, "missing.rs", ConflictChoice::Ours).is_err());
        assert_eq!(
            resolution_plan(&[]),
            Err("This commit has no files to resolve.")
        );
    }

    #[test]
    fn apply_unapply_and_delete_a_local_branch() {
        let (backend, mut store) = ready();
        drain!(&backend, store, store.open_branches());
        assert!(store.pages.branches_open);
        let names: Vec<_> = store
            .pages
            .visible_branches()
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert!(names.contains(&"topic".to_string()));
        assert!(names.contains(&"lane-a".to_string()));
        store.set_branch_filter("topic".to_string());
        assert_eq!(store.pages.selected_branch().unwrap().name, "topic");
        assert!(!store.pages.selected_branch().unwrap().applied);
        drain!(&backend, store, store.apply_named("topic").unwrap());
        assert!(
            store
                .workspace
                .as_ref()
                .unwrap()
                .stacks
                .iter()
                .any(|stack| stack.names().any(|name| name == "topic"))
        );
        store.set_branch_filter(String::new());
        store.set_branch_tab(BranchTab::Local);
        store.place_branch("topic");
        assert!(store.pages.selected_branch().unwrap().applied);
        drain!(&backend, store, store.unapply_named(Some("topic")).unwrap());
        assert!(
            store
                .workspace
                .as_ref()
                .unwrap()
                .stacks
                .iter()
                .all(|stack| stack.names().all(|name| name != "topic"))
        );
        store.place_branch("topic");
        assert!(!store.pages.selected_branch().unwrap().applied);
        store.ask_delete_branch(Some("topic")).unwrap();
        assert!(store.pages.ask.as_ref().unwrap().destructive);
        drain!(&backend, store, store.confirm_page_yes());
        assert!(store.find_listed("topic").is_none());
        assert!(store.pages.branch_busy.is_none());
    }

    #[test]
    fn remote_only_branch_cannot_be_applied_or_deleted() {
        let (backend, mut store) = ready();
        drain!(&backend, store, store.open_branches());
        store.set_branch_tab(BranchTab::Remote);
        let effect = store.apply_named("remote-only").unwrap();
        drain!(&backend, store, effect);
        assert!(store.pages.branch_notice_bad);
        assert!(
            store
                .pages
                .branch_notice
                .as_deref()
                .unwrap()
                .contains("remote")
        );
        let error = store.delete_named(Some("remote-only")).unwrap_err();
        assert!(error.contains("local"));
    }

    #[test]
    fn snapshot_create_and_restore_round_trip() {
        let (backend, mut store) = ready();
        let before = store.workspace.clone().unwrap();
        drain!(
            &backend,
            store,
            store.create_snapshot_now("Checkpoint").unwrap()
        );
        let sha = store.pages.snapshot_sha.clone().unwrap();
        assert_eq!(store.pages.selected_snapshot().unwrap().title, "Checkpoint");
        assert!(store.pages.selected_snapshot().unwrap().restorable);
        drain!(&backend, store, store.open_branches());
        drain!(&backend, store, store.apply_named("topic").unwrap());
        assert!(store.workspace.as_ref().unwrap().stacks.len() > before.stacks.len());
        store.pages.history_open = true;
        store.pages.snapshots = backend.list_snapshots("demo-project", 50).unwrap();
        store.select_snapshot_id(&sha).unwrap();
        drain!(&backend, store, store.restore_selected().unwrap());
        assert_eq!(
            store.workspace.as_ref().unwrap().stacks.len(),
            before.stacks.len()
        );
        assert!(store.pages.history_open);
    }

    #[test]
    fn edit_enter_save_and_abort() {
        let (backend, mut store) = ready();
        let sha = store.workspace.as_ref().unwrap().stacks[0].branches[0].commits[0]
            .sha
            .clone();
        drain!(&backend, store, store.request_enter_edit(&sha).unwrap());
        assert_eq!(store.pages.phase, EditPhase::Editing);
        assert_eq!(store.pages.edit_short.as_deref(), Some("a11a11a"));
        assert_eq!(store.mutation_blocked(), Some(EDIT_LOCK));
        store.apply_key("c");
        assert!(!store.composer.open);
        assert_eq!(store.toast.as_deref(), Some(EDIT_LOCK));
        drain!(&backend, store, store.request_save_edit().unwrap());
        assert_eq!(store.pages.phase, EditPhase::Idle);
        assert!(store.mutation_blocked().is_none());

        let other = store.workspace.as_ref().unwrap().stacks[2].branches[0].commits[0]
            .sha
            .clone();
        drain!(&backend, store, store.request_enter_edit(&other).unwrap());
        assert!(store.pages.editing());
        drain!(&backend, store, store.request_abort_edit().unwrap());
        assert_eq!(store.pages.phase, EditPhase::Idle);
        assert!(store.edit_strip_text().is_empty());
    }

    #[test]
    fn resolve_conflict_clears_the_commit() {
        let (backend, mut store) = ready();
        let sha = store
            .workspace
            .as_ref()
            .unwrap()
            .stacks
            .iter()
            .find_map(|stack| {
                stack.branches.iter().find_map(|branch| {
                    branch
                        .commits
                        .iter()
                        .find(|commit| commit.conflicted)
                        .map(|commit| commit.sha.clone())
                })
            })
            .unwrap();
        drain!(&backend, store, store.open_conflicts(Some(&sha)).unwrap());
        assert!(store.pages.conflicts_open);
        assert_eq!(store.pages.conflict_files[0].path, "src/helper.rs");
        store.choose_side("src/helper.rs", "ours").unwrap();
        drain!(&backend, store, store.mark_resolved().unwrap());
        assert!(!store.pages.conflicts_open);
        assert!(
            store
                .workspace
                .as_ref()
                .unwrap()
                .stacks
                .iter()
                .all(|stack| {
                    stack
                        .branches
                        .iter()
                        .all(|branch| branch.commits.iter().all(|commit| !commit.conflicted))
                })
        );
    }

    #[test]
    fn switching_pages_keeps_filter_selection_and_scroll() {
        let (_backend, mut store) = ready();
        store.pages.filter = "lane".to_string();
        store.pages.branch_cursor = 2;
        store.pages.branch_scroll = 48;
        store.pages.snapshot_cursor = 1;
        store.pages.history_scroll = 20;
        store.show_page(AppPage::Branches);
        assert_eq!(store.app_page(), AppPage::Branches);
        assert_eq!(store.pages.filter, "lane");
        assert_eq!(store.pages.branch_cursor, 2);
        assert_eq!(store.pages.branch_scroll, 48);
        store.show_page(AppPage::History);
        assert_eq!(store.app_page(), AppPage::History);
        assert!(!store.pages.branches_open);
        assert_eq!(store.pages.filter, "lane");
        assert_eq!(store.pages.branch_scroll, 48);
        assert_eq!(store.pages.snapshot_cursor, 1);
        assert_eq!(store.pages.history_scroll, 20);
        store.show_page(AppPage::Workspace);
        assert_eq!(store.app_page(), AppPage::Workspace);
        assert_eq!(store.pages.history_scroll, 20);
        store.apply_key("escape");
        assert_eq!(store.app_page(), AppPage::Workspace);
        store.apply_key("b");
        assert_eq!(store.app_page(), AppPage::Branches);
        assert_eq!(store.pages.filter, "lane");
        store.apply_key("escape");
        assert_eq!(store.app_page(), AppPage::Workspace);
        store.apply_key("h");
        assert_eq!(store.app_page(), AppPage::History);
        store.apply_key("b");
        assert_eq!(store.app_page(), AppPage::Branches);
        assert_eq!(store.pages.history_scroll, 20);
    }

    #[test]
    fn listed_branch_menu_marks_delete_and_explains_a_disabled_row() {
        let (_backend, mut store) = ready();
        store.pages.branch_rows = sample();
        store.open_listed_branch("lane-a");
        let entries = store.menu_entries();
        let delete = entries
            .iter()
            .find(|entry| entry.label == "Delete")
            .unwrap();
        assert!(delete.destructive);
        assert!(!delete.enabled);
        assert!(!delete.tip.is_empty());
        let apply = entries
            .iter()
            .find(|entry| entry.label == "Apply to workspace")
            .unwrap();
        assert!(!apply.enabled);
        assert!(apply.tip.contains("already"));
        store.open_listed_branch("topic");
        let entries = store.menu_entries();
        let delete = entries
            .iter()
            .find(|entry| entry.label == "Delete")
            .unwrap();
        assert!(delete.enabled && delete.destructive);
        let unapply = entries
            .iter()
            .find(|entry| entry.label == "Unapply")
            .unwrap();
        assert!(!unapply.enabled);
        assert!(unapply.tip.contains("not in the workspace"));
    }
}
