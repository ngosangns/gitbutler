//! Window state with no GPUI types. The view applies [`Loaded`] on the UI thread
//! after `load` finishes on a background thread.
//!
//! The workspace page is three columns: uncommitted files, the diff, and branches.
//! A focused branch or commit splits the branches column. Branches and history
//! are their own pages. The project list is a menu, not a column.
//!
//! One window entity owns this store. Selectors and virtual lists keep a frame
//! from rebuilding every row. A notify still wakes that one entity.

mod activity;
mod diffrows;
mod forge;
mod layout;
mod mutate;
mod pages;
mod perf;
mod remote;
mod settings;
mod shortcuts;

pub use activity::{Activity, Surface, ToastView, spec as operation_spec};
pub use diffrows::{DiffRow, RowKind, build_diff_rows};
pub use forge::{ForgeDone, ForgeOp, REPLACE_PROMPT, perform_forge, perform_suggest};
pub use layout::{
    COLUMN_DEFAULT, HANDLE_WIDTH, clamp_popup, column_pixels, drag_handle, load_fractions,
    persistent_file, save_fractions,
};
pub use mutate::{
    Action, ActionMenu, BusyOp, Composer, Field, MutationDone, PendingConfirm, RewordDraft,
    perform_action,
};
#[cfg(any(test, feature = "devtools"))]
pub use pages::side_name;
pub use pages::{
    AppPage, BranchTab, ConflictFile, EditPhase, PageDone, PageOp, Pages, file_change_label,
    perform_page, snapshot_kind, snapshot_when,
};
pub use perf::{
    DIFF_PAGE, REFRESH_QUIET_MS, RefreshGate, measure as perf_measure, seed_detail_workspace,
    seed_diff, seed_files, seed_git_event, seed_large_branches, seed_large_files,
    seed_large_reviews, seed_large_snapshot_files, seed_large_workspace, seed_snapshots,
    seed_workspace, visible_window,
};
pub use remote::{
    PUSH_CHOICES, RemoteDone, RemoteOp, RemoteUi, askpass_label, perform_remote, push_choice_tip,
};
pub use settings::{SettingsDone, SettingsOp, SettingsSection, SettingsUi, perform_settings};
pub use shortcuts::shortcut_rows;

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use gb_bridge::{
    Backend, BranchRow, BridgeEvent, CommitDetails, CommitSummary, Compat, DiffRequest,
    Environment, FileChange, ForgeStatus, ProjectSummary, Review, SelectionSpec, WorkspaceSummary,
    WorktreeView,
};

/// How many columns the body draws. The project list is a menu, not a column.
#[cfg(any(test, feature = "devtools"))]
pub const COLUMNS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Uncommitted,
    Changes,
    Branches,
    Detail,
}

impl Pane {
    #[cfg(feature = "devtools")]
    pub fn name(self) -> &'static str {
        match self {
            Pane::Uncommitted => "uncommitted",
            Pane::Changes => "changes",
            Pane::Branches => "branches",
            Pane::Detail => "detail",
        }
    }

    #[cfg(feature = "devtools")]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "uncommitted" | "files" => Some(Pane::Uncommitted),
            "changes" | "diff" => Some(Pane::Changes),
            "branches" | "lanes" => Some(Pane::Branches),
            "detail" | "branch-detail" => Some(Pane::Detail),
            _ => None,
        }
    }

    fn index(self) -> usize {
        match self {
            Pane::Uncommitted => 0,
            Pane::Changes => 1,
            Pane::Branches => 2,
            Pane::Detail => 3,
        }
    }
}

/// List is flat and grouped by lane. Tree nests directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    List,
    Tree,
}

impl ViewMode {
    #[cfg(feature = "devtools")]
    pub fn name(self) -> &'static str {
        match self {
            ViewMode::List => "list",
            ViewMode::Tree => "tree",
        }
    }

    #[cfg(feature = "devtools")]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "list" => Some(ViewMode::List),
            "tree" => Some(ViewMode::Tree),
            _ => None,
        }
    }
}

/// What the branches column is holding open in its bottom half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideFocus {
    None,
    Branch { stack: usize, branch: usize },
    Commit { sha: String },
}

/// Which file the Changes column is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShownFile {
    None,
    Worktree { path: String },
    Commit { sha: String, path: String },
}

/// Reported by the dev socket. A commit or branch wins over a file, because
/// that is what opens the bottom pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(test, feature = "devtools"))]
pub enum FocusedKind {
    None,
    File,
    Branch,
    Commit,
}

#[cfg(any(test, feature = "devtools"))]
impl FocusedKind {
    #[cfg(feature = "devtools")]
    pub fn name(self) -> &'static str {
        match self {
            FocusedKind::None => "none",
            FocusedKind::File => "file",
            FocusedKind::Branch => "branch",
            FocusedKind::Commit => "commit",
        }
    }
}

/// Work the view does after a key or a click. The store has already moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Refresh,
    SelectProject(String),
    ShowCommit(String),
    ShowDiff(DiffRequest),
    Mutate(Action),
    Remote(RemoteOp),
    Page(PageOp),
    Forge(ForgeOp),
    Suggest(Vec<String>),
    Settings(SettingsOp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub env: Environment,
    pub projects: Vec<ProjectSummary>,
    pub list_reason: Option<String>,
    pub workspace: Option<WorkspaceSummary>,
    pub worktree: Option<WorktreeView>,
    pub forge: ForgeStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeGroup {
    pub name: String,
    pub files: Vec<usize>,
}

/// One row in the uncommitted list. Headers and files share one height.
/// A header shares its member paths so a frame never clones them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSlot {
    Group {
        name: String,
        files: Vec<usize>,
        paths: Arc<[String]>,
    },
    File {
        flat: usize,
        index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneItem {
    pub stack: usize,
    pub branch: usize,
    /// `None` is the branch header.
    pub commit: Option<usize>,
}

/// One directory or file in the uncommitted tree, before collapse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub name: String,
    pub path: String,
    /// Index into the worktree file list. `None` is a directory.
    pub file: Option<usize>,
    pub additions: u32,
    pub deletions: u32,
    pub file_count: usize,
    pub children: Vec<TreeNode>,
}

/// One visible tree row. Children of a collapsed directory are omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub file: Option<usize>,
    pub additions: u32,
    pub deletions: u32,
    pub file_count: usize,
    pub expanded: bool,
}

/// Facts for the bottom pane when a branch is focused.
/// Ahead/behind is omitted: the bridge branch has no upstream counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchFocusInfo {
    pub name: String,
    pub tip_sha: String,
    pub tip_full: String,
    pub stack_label: String,
    pub commit_count: usize,
    pub push_state: Option<String>,
    pub authors: Vec<String>,
    pub earliest: Option<i64>,
    pub latest: Option<i64>,
    /// Shared with the virtualized commit list instead of cloned per frame.
    pub commits: Arc<Vec<CommitLine>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitLine {
    pub sha: String,
    pub short_sha: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub env: Option<Environment>,
    pub projects: Vec<ProjectSummary>,
    pub list_reason: Option<String>,
    pub selected: Option<String>,
    /// Shared so a frame can keep the graph without cloning every commit.
    pub workspace: Option<Arc<WorkspaceSummary>>,
    /// Shared so a frame can keep the file list without cloning every path.
    pub worktree: Option<Arc<WorktreeView>>,
    pub error: Option<String>,
    pub loading: bool,
    pub refreshing: bool,
    pub last_refresh: Option<SystemTime>,
    pub workspace_stale: bool,
    pub watching: bool,
    pub exclusive: Option<bool>,
    pub change_count: u64,
    pub pane: Pane,
    cursors: [usize; 4],
    pub collapsed: BTreeSet<String>,
    pub dirs_collapsed: BTreeSet<String>,
    pub view: ViewMode,
    pub side: SideFocus,
    pub shown: ShownFile,
    pub project_menu: bool,
    project_cursor: usize,
    project_hints: BTreeMap<String, String>,
    pub commit_loading: bool,
    pub diff_loading: bool,
    pub commit_error: Option<String>,
    pub diff_error: Option<String>,
    pub commit: Option<CommitDetails>,
    pub diff_rows: Arc<Vec<DiffRow>>,
    pub hunk_cursor: usize,
    pub help: bool,
    scroll_to: Option<usize>,
    pub marked: BTreeSet<String>,
    mark_anchor: Option<String>,
    pub marked_hunks: BTreeSet<(String, String)>,
    pub menu: Option<ActionMenu>,
    pub confirm: Option<PendingConfirm>,
    pub composer: Composer,
    pub reword: Option<RewordDraft>,
    pub busy: Option<BusyOp>,
    pub mutation_error: Option<String>,
    pub toast: Option<String>,
    /// False for a result such as "Pushed 2 commits". Errors stay red.
    pub toast_bad: bool,
    pub remote: RemoteUi,
    pub pages: Pages,
    pub forge: forge::ForgeUi,
    pub suggest_busy: bool,
    /// A suggestion waiting until the user replaces text they already typed.
    pub message_hold: Option<gb_bridge::SuggestedMessage>,
    pub settings: SettingsUi,
    /// Share of the workspace row for Uncommitted, Changes, and Branches.
    pub column_fractions: [f32; 3],
    /// Shared loading, success, and error phases. The view reads this.
    pub activity: Activity,
    /// How many diff rows to build. Show more raises it by one page.
    pub diff_limit: usize,
    /// Watcher events coalesce here. The window polls it.
    pub refresh_gate: RefreshGate,
    undo_labels: Vec<String>,
    derived: RefCell<Derived>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            env: None,
            projects: Vec::new(),
            list_reason: None,
            selected: None,
            workspace: None,
            worktree: None,
            error: None,
            loading: false,
            refreshing: false,
            last_refresh: None,
            workspace_stale: false,
            watching: false,
            exclusive: None,
            change_count: 0,
            pane: Pane::Uncommitted,
            cursors: [0; 4],
            collapsed: BTreeSet::new(),
            dirs_collapsed: BTreeSet::new(),
            view: ViewMode::List,
            side: SideFocus::None,
            shown: ShownFile::None,
            project_menu: false,
            project_cursor: 0,
            project_hints: BTreeMap::new(),
            commit_loading: false,
            diff_loading: false,
            commit_error: None,
            diff_error: None,
            commit: None,
            diff_rows: Arc::new(Vec::new()),
            hunk_cursor: 0,
            help: false,
            scroll_to: None,
            marked: BTreeSet::new(),
            mark_anchor: None,
            marked_hunks: BTreeSet::new(),
            menu: None,
            confirm: None,
            composer: Composer::default(),
            reword: None,
            busy: None,
            mutation_error: None,
            toast: None,
            toast_bad: true,
            remote: RemoteUi::default(),
            pages: Pages::default(),
            forge: forge::ForgeUi::default(),
            suggest_busy: false,
            message_hold: None,
            settings: SettingsUi::default(),
            column_fractions: COLUMN_DEFAULT,
            undo_labels: Vec::new(),
            activity: Activity::new(),
            diff_limit: perf::DIFF_PAGE,
            refresh_gate: RefreshGate::default(),
            derived: RefCell::new(Derived::default()),
        }
    }
}

/// Sorted worktree file paths with their file indexes. One build answers the
/// folder ranges (`dir/src/` runs), exact-path lookups, and full-folder marks
/// without scanning or cloning per frame. Rebuilt only when the worktree is
/// replaced.
#[derive(Debug, Default, Clone)]
struct FolderIndex {
    /// Every file path, sorted, parallel to `files`.
    paths: Vec<String>,
    /// Index into `WorktreeView::files` for each entry of `paths`.
    files: Vec<usize>,
}

impl FolderIndex {
    fn build(files: &[FileChange]) -> Self {
        let mut entries: Vec<(String, usize)> = files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.path.clone(), index))
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Self {
            paths: entries.iter().map(|(path, _)| path.clone()).collect(),
            files: entries.into_iter().map(|(_, index)| index).collect(),
        }
    }

    fn lookup(&self, path: &str) -> Option<usize> {
        let position = self
            .paths
            .binary_search_by(|probe| probe.as_str().cmp(path))
            .ok()?;
        Some(self.files[position])
    }

    /// Half-open range of `paths` under `folder` (paths starting `folder/`).
    /// The block is exactly `[folder/, folder0)` because `/` is 0x2F and `0`
    /// is the next byte, so a plain bounds search stays equivalent to
    /// `starts_with("{folder}/")` for every folder, including one that
    /// already ends in a slash.
    fn range(&self, folder: &str) -> std::ops::Range<usize> {
        let lower = format!("{folder}/");
        let upper = format!("{folder}0");
        let start = self
            .paths
            .partition_point(|path| path.as_str() < lower.as_str());
        let end = self
            .paths
            .partition_point(|path| path.as_str() < upper.as_str());
        start..end
    }
}

/// One branch row's five searchable fields, lowercased once per data load.
type LoweredBranch = [String; 5];

/// Pre-lowered branch rows, keyed on (branch generation, length, address)
/// of `pages.branch_rows`. The generation keeps a freed address from serving
/// stale lowered fields.
type BranchLcCache = ((u64, usize, usize), Arc<Vec<LoweredBranch>>);

/// Cached filtered reviews, keyed on (forge generation, committed filter).
type ReviewCache = ((u64, String), Arc<Vec<Review>>);

/// `path -> marked hunk headers`, built once per mark change so the diff can
/// probe `hunk_marked` without allocating a key per visible row.
type HunkMarkIndex = HashMap<Box<str>, HashSet<Box<str>>>;

/// Everything the next commit would take, keyed on the inputs that can change
/// it: the mark generations, the data generation, and the composer lane.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpecsKey {
    mark_gen: u64,
    data_gen: u64,
    worktree: usize,
    workspace: usize,
    new_lane: bool,
    lane: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Derived {
    /// Bumped whenever the workspace or worktree is replaced, so cache keys
    /// never rely on `Arc` addresses alone (an address can be reused).
    data_gen: u64,
    file_key: Option<(u64, usize)>,
    groups: Vec<ChangeGroup>,
    flat: Vec<usize>,
    slots: Arc<Vec<FileSlot>>,
    group_rebuilds: u64,
    tree_key: Option<(u64, usize, u64)>,
    tree_rows: Arc<Vec<TreeRow>>,
    folder_key: Option<(u64, usize)>,
    folders: Arc<FolderIndex>,
    lane_key: Option<(u64, usize, u64)>,
    lane_rows: Arc<Vec<LaneItem>>,
    lane_rebuilds: u64,
    branch_key: Option<u64>,
    branch_indexes: Arc<Vec<usize>>,
    branch_filter_rebuilds: u64,
    /// Bumped whenever `pages.branch_rows` is replaced, so cache keys never
    /// rely on the row `Vec` address alone.
    branch_gen: u64,
    /// The filter the branch lists are actually filtered with. Typing only
    /// moves `pages.filter`; a debounce commits it here.
    branch_filter: String,
    branch_lc: Option<BranchLcCache>,
    branch_visible_key: Option<u64>,
    branch_visible: Arc<Vec<BranchRow>>,
    forge_gen: u64,
    reviews: Option<ReviewCache>,
    mark_gen: u64,
    hunk_marks: Option<(u64, Arc<HunkMarkIndex>)>,
    labels_key: Option<(u64, usize)>,
    labels: Arc<Vec<String>>,
    specs_key: Option<SpecsKey>,
    specs: Arc<Vec<SelectionSpec>>,
    info_key: Option<(u64, usize, usize, usize)>,
    info: Option<BranchFocusInfo>,
    headers_key: Option<(u64, usize, String)>,
    headers: Arc<Vec<String>>,
    branch_name_key: Option<(u64, usize, String)>,
    branch_name: Option<Option<String>>,
}

/// Test fixture: shared data set, everything else default. Lets tests outside
/// this module build a store despite the private fields.
#[cfg(test)]
impl Store {
    pub fn seeded(
        worktree: Option<Arc<WorktreeView>>,
        workspace: Option<Arc<WorkspaceSummary>>,
    ) -> Self {
        Self {
            worktree,
            workspace,
            ..Store::default()
        }
    }
}

impl Store {
    pub fn select(&mut self, id: Option<String>) {
        if self.selected != id {
            self.bump_data();
            self.workspace = None;
            self.worktree = None;
            self.error = None;
            self.clear_focus();
            self.cursors = [0; 4];
            self.collapsed.clear();
            self.dirs_collapsed.clear();
            self.project_menu = false;
            self.pane = Pane::Uncommitted;
            self.reset_mutation_ui();
            self.remote.on_project_change();
            self.on_pages_project_change();
            self.reset_forge();
            self.close_settings();
            self.settings.project = None;
            self.settings.project_error = None;
        }
        self.selected = id;
    }

    pub fn set_watch(&mut self, active: Option<bool>) {
        match active {
            Some(exclusive) => {
                self.watching = true;
                self.exclusive = Some(exclusive);
            }
            None => {
                self.watching = false;
                self.exclusive = None;
            }
        }
    }

    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
        self.sync_hints();
    }

    pub fn apply(&mut self, loaded: Loaded) {
        self.bump_data();
        let previous = self.worktree.clone();
        let failed = loaded.error.clone();
        self.env = Some(loaded.env);
        self.projects = loaded.projects;
        self.list_reason = loaded.list_reason;
        self.workspace = loaded.workspace.map(Arc::new);
        self.error = loaded.error;
        self.loading = false;
        self.refreshing = false;
        self.workspace_stale = false;
        self.last_refresh = Some(SystemTime::now());
        self.worktree = loaded.worktree.map(|mut worktree| {
            if let Some(previous) = previous.as_deref() {
                worktree.keep_assignments_from(previous);
            }
            Arc::new(worktree)
        });
        if let Some(id) = &self.selected
            && !self.projects.iter().any(|project| &project.id == id)
        {
            self.selected = None;
            self.workspace = None;
            self.worktree = None;
        }
        self.apply_forge_status(loaded.forge);
        self.sync_hints();
        self.clamp();
        self.settle_refresh(failed);
    }

    /// `Worktree` replaces the file list. Git head, activity, and workspace
    /// activity only mark the commit graph stale. The window's refresh gate
    /// decides when that stale flag becomes a fetch.
    pub fn apply_event(&mut self, event: BridgeEvent) {
        self.change_count += 1;
        match event {
            BridgeEvent::Worktree(view) => {
                self.bump_data();
                self.worktree = Some(Arc::new(view))
            }
            other if other.refreshes_workspace() => self.workspace_stale = true,
            _ => {}
        }
        self.clamp();
    }

    pub fn apply_key(&mut self, key: &str) -> Effect {
        self.apply_key_handled(key).0
    }

    /// `apply_key` plus whether the key reached a handler. An unhandled key
    /// changed no state, so the window can skip its notify and let the
    /// keystroke propagate to OS hotkeys.
    pub fn apply_key_handled(&mut self, key: &str) -> (Effect, bool) {
        let key = if key == "esc" { "escape" } else { key };
        if self.remote.askpass.is_some()
            && let Some(effect) = self.apply_remote_key(key)
        {
            return (effect, true);
        }
        if let Some(effect) = self.apply_settings_key(key) {
            return (effect, true);
        }
        if key == "cmd-comma" {
            return (self.toggle_settings(), true);
        }
        if let Some(effect) = self.apply_page_key(key) {
            return (effect, true);
        }
        if let Some(effect) = self.apply_remote_key(key) {
            return (effect, true);
        }
        if let Some(effect) = self.apply_mutation_key(key) {
            return (effect, true);
        }
        match key {
            "cmd-p" | "cmd-P" => {
                self.toggle_project_menu();
                return (Effect::None, true);
            }
            "t" => {
                self.toggle_view();
                return (Effect::None, true);
            }
            "," => return (self.toggle_settings(), true),
            "g" => return (self.request_suggestion(), true),
            _ => {}
        }
        if self.project_menu {
            return match key {
                "up" => {
                    self.move_project(-1);
                    (Effect::None, true)
                }
                "down" => {
                    self.move_project(1);
                    (Effect::None, true)
                }
                "enter" => (self.choose_project(), true),
                "escape" => (self.go_back(), true),
                "cmd-r" | "cmd-R" => (Effect::Refresh, true),
                _ => (Effect::None, false),
            };
        }
        match key {
            "up" => {
                self.move_cursor(-1);
                (Effect::None, true)
            }
            "down" => {
                self.move_cursor(1);
                (Effect::None, true)
            }
            "left" | "shift-tab" => {
                self.cycle(-1);
                (Effect::None, true)
            }
            "right" | "tab" => {
                self.cycle(1);
                (Effect::None, true)
            }
            "enter" => (self.open_current(), true),
            "escape" => (self.go_back(), true),
            "j" if self.diff_focused() => {
                self.move_hunk(1);
                (Effect::None, true)
            }
            "k" if self.diff_focused() => {
                self.move_hunk(-1);
                (Effect::None, true)
            }
            "j" | "k" => (Effect::None, false),
            "cmd-r" | "cmd-R" => (Effect::Refresh, true),
            "cmd-shift-f" | "cmd-shift-F" => (self.request_fetch(), true),
            _ => (Effect::None, false),
        }
    }

    pub fn open_current(&mut self) -> Effect {
        match self.pane {
            Pane::Uncommitted => self.open_uncommitted(),
            Pane::Changes => Effect::None,
            Pane::Branches => self.open_lane_row(),
            Pane::Detail => self.open_detail(),
        }
    }

    pub fn toggle_help(&mut self) {
        self.help = !self.help;
        if self.help {
            self.close_settings();
        }
    }

    pub fn toggle_project_menu(&mut self) {
        self.set_project_menu(!self.project_menu);
    }

    pub fn set_project_menu(&mut self, open: bool) {
        self.project_menu = open;
        if !open {
            return;
        }
        if let Some(id) = &self.selected
            && let Some(index) = self.projects.iter().position(|project| &project.id == id)
        {
            self.project_cursor = index;
        }
        self.clamp_project_cursor();
    }

    pub fn project_cursor(&self) -> usize {
        self.project_cursor
    }

    /// Pick the menu row at `index`. A version mismatch leaves the menu open.
    pub fn choose_project_at(&mut self, index: usize) -> Effect {
        self.project_cursor = index;
        self.clamp_project_cursor();
        self.choose_project()
    }

    pub fn toggle_view(&mut self) {
        let next = match self.view {
            ViewMode::List => ViewMode::Tree,
            ViewMode::Tree => ViewMode::List,
        };
        self.set_view(next);
    }

    pub fn set_view(&mut self, mode: ViewMode) {
        if self.view == mode {
            return;
        }
        let path = self.uncommitted_path().or_else(|| match &self.shown {
            ShownFile::Worktree { path } => Some(path.clone()),
            ShownFile::None | ShownFile::Commit { .. } => None,
        });
        self.view = mode;
        if mode == ViewMode::Tree
            && let Some(path) = &path
        {
            self.reveal_ancestors(path);
        }
        if let Some(path) = path {
            self.place_uncommitted(&path);
        } else {
            self.clamp();
        }
    }

    pub fn cursor(&self, pane: Pane) -> usize {
        self.cursors[pane.index()]
    }

    pub fn set_cursor(&mut self, pane: Pane, index: usize) {
        self.pane = pane;
        self.cursors[pane.index()] = index;
        self.clamp();
    }

    pub fn take_scroll(&mut self) -> Option<usize> {
        self.scroll_to.take()
    }

    pub fn hunk_row(&self) -> Option<usize> {
        self.diff_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind == RowKind::Hunk)
            .nth(self.hunk_cursor)
            .map(|(index, _)| index)
    }

    pub fn panes(&self) -> Vec<Pane> {
        let mut panes = vec![Pane::Uncommitted, Pane::Changes, Pane::Branches];
        if self.detail_open() {
            panes.push(Pane::Detail);
        }
        panes
    }

    pub fn detail_open(&self) -> bool {
        !matches!(self.side, SideFocus::None)
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn focused_kind(&self) -> FocusedKind {
        match &self.side {
            SideFocus::Commit { .. } => FocusedKind::Commit,
            SideFocus::Branch { .. } => FocusedKind::Branch,
            SideFocus::None => match &self.shown {
                ShownFile::None => FocusedKind::None,
                ShownFile::Worktree { .. } | ShownFile::Commit { .. } => FocusedKind::File,
            },
        }
    }

    #[cfg(feature = "devtools")]
    pub fn diff_ready(&self) -> bool {
        !self.diff_loading
            && self.diff_error.is_none()
            && !matches!(self.shown, ShownFile::None)
            && !self.diff_rows.is_empty()
    }

    #[cfg(feature = "devtools")]
    pub fn commit_ready(&self) -> bool {
        let SideFocus::Commit { sha } = &self.side else {
            return false;
        };
        !self.commit_loading
            && self
                .commit
                .as_ref()
                .is_some_and(|commit| commit.sha == *sha || commit.short_sha == *sha)
    }

    fn bump_data(&mut self) {
        let mut derived = self.derived.borrow_mut();
        derived.data_gen = derived.data_gen.wrapping_add(1);
    }

    fn bump_branches(&mut self) {
        let mut derived = self.derived.borrow_mut();
        derived.branch_gen = derived.branch_gen.wrapping_add(1);
    }

    pub(crate) fn bump_marks(&self) {
        let mut derived = self.derived.borrow_mut();
        derived.mark_gen = derived.mark_gen.wrapping_add(1);
    }

    pub(crate) fn bump_forge(&mut self) {
        let mut derived = self.derived.borrow_mut();
        derived.forge_gen = derived.forge_gen.wrapping_add(1);
    }

    /// Replace the worktree. Every data assignment must bump the matching
    /// generation: a freed `Arc` address can be reused and would serve stale
    /// cached rows otherwise.
    pub fn set_worktree(&mut self, worktree: Option<Arc<WorktreeView>>) {
        self.bump_data();
        self.worktree = worktree;
    }

    /// Replace the workspace. See [`Store::set_worktree`].
    pub fn set_workspace(&mut self, workspace: Option<Arc<WorkspaceSummary>>) {
        self.bump_data();
        self.workspace = workspace;
    }

    /// Replace the branch-page rows. See [`Store::set_worktree`].
    pub fn set_branch_rows(&mut self, rows: Vec<BranchRow>) {
        self.bump_branches();
        self.pages.branch_rows = rows;
    }

    /// Replace the forge review list. See [`Store::set_worktree`].
    pub fn set_reviews(&mut self, reviews: Vec<Review>) {
        self.bump_forge();
        self.forge.reviews = reviews;
    }

    /// Replace the file and hunk marks. See [`Store::set_worktree`].
    pub fn set_marks(
        &mut self,
        marked: BTreeSet<String>,
        marked_hunks: BTreeSet<(String, String)>,
    ) {
        self.bump_marks();
        self.marked = marked;
        self.marked_hunks = marked_hunks;
    }

    fn worktree_key(&self) -> (u64, usize) {
        let data_gen = self.derived.borrow().data_gen;
        (data_gen, arc_key(self.worktree.as_ref()))
    }

    fn workspace_key(&self) -> (u64, usize) {
        let data_gen = self.derived.borrow().data_gen;
        (data_gen, arc_key(self.workspace.as_ref()))
    }

    /// Stamp for the branch caches: committed filter, tab, row identity, and
    /// the branch generation (a freed row address can be reused).
    fn branch_stamp(&self) -> u64 {
        let (committed, branch_gen) = {
            let derived = self.derived.borrow();
            (derived.branch_filter.clone(), derived.branch_gen)
        };
        branch_stamp(&self.pages, &committed)
            .wrapping_add(branch_gen.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    /// `visible_indices` over pre-lowered rows; the query is lowercased once.
    fn visible_indices_fast(&self) -> Vec<usize> {
        let rows: &[BranchRow] = &self.pages.branch_rows;
        let tab = self.pages.tab;
        let committed = self.derived.borrow().branch_filter.clone();
        let query = committed.trim();
        if query.is_empty() {
            return rows
                .iter()
                .enumerate()
                .filter(|(_, row)| pages::branch_on_tab(row, tab))
                .map(|(index, _)| index)
                .collect();
        }
        let lowered = query.to_ascii_lowercase();
        let fields = self.branch_lc();
        rows.iter()
            .zip(fields.iter())
            .enumerate()
            .filter(|(_, (row, fields))| {
                pages::branch_on_tab(row, tab)
                    && fields.iter().any(|field| field.contains(&lowered))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// The five searchable branch fields, lowercased once per data load.
    fn branch_lc(&self) -> Arc<Vec<LoweredBranch>> {
        let key = {
            let branch_gen = self.derived.borrow().branch_gen;
            (
                branch_gen,
                self.pages.branch_rows.len(),
                self.pages.branch_rows.as_ptr() as usize,
            )
        };
        if let Some((have, fields)) = self.derived.borrow().branch_lc.as_ref()
            && *have == key
        {
            return Arc::clone(fields);
        }
        let fields: Arc<Vec<LoweredBranch>> = Arc::new(
            self.pages
                .branch_rows
                .iter()
                .map(|row| {
                    [
                        row.name.to_ascii_lowercase(),
                        row.last_commit.to_ascii_lowercase(),
                        row.author.to_ascii_lowercase(),
                        row.ref_name.to_ascii_lowercase(),
                        row.tip_short.to_ascii_lowercase(),
                    ]
                })
                .collect(),
        );
        self.derived.borrow_mut().branch_lc = Some((key, Arc::clone(&fields)));
        fields
    }

    /// `path -> marked headers`, rebuilt only when a mark changes.
    pub(crate) fn hunk_mark_index(&self) -> Arc<HunkMarkIndex> {
        let stamp = self.derived.borrow().mark_gen;
        if let Some((have, index)) = self.derived.borrow().hunk_marks.as_ref()
            && *have == stamp
        {
            return Arc::clone(index);
        }
        let mut index: HunkMarkIndex = HashMap::new();
        for (path, header) in &self.marked_hunks {
            index
                .entry(path.as_str().into())
                .or_default()
                .insert(header.as_str().into());
        }
        let index = Arc::new(index);
        self.derived.borrow_mut().hunk_marks = Some((stamp, Arc::clone(&index)));
        index
    }

    /// Hunk headers of one worktree file, shared with the diff pane instead of
    /// a per-frame deep clone.
    pub fn file_hunk_headers(&self, path: &str) -> Arc<Vec<String>> {
        let key = {
            let (data_gen, worktree) = self.worktree_key();
            (data_gen, worktree, path.to_string())
        };
        if self.derived.borrow().headers_key.as_ref() == Some(&key) {
            return Arc::clone(&self.derived.borrow().headers);
        }
        let headers = self
            .file_index_by_path(path)
            .and_then(|index| self.worktree.as_ref()?.files.get(index))
            .map(|file| file.hunks.iter().map(|hunk| hunk.header.clone()).collect())
            .unwrap_or_default();
        let headers = Arc::new(headers);
        let mut derived = self.derived.borrow_mut();
        derived.headers_key = Some(key);
        derived.headers = Arc::clone(&headers);
        headers
    }

    /// Chip text for every worktree file, parallel to `files`. Built once per
    /// worktree generation so visible rows never rescan hunk lanes.
    pub fn lane_labels(&self) -> Arc<Vec<String>> {
        let key = self.worktree_key();
        if self.derived.borrow().labels_key == Some(key) {
            return Arc::clone(&self.derived.borrow().labels);
        }
        let labels: Arc<Vec<String>> = Arc::new(match self.worktree.as_deref() {
            Some(worktree) => worktree.files.iter().map(assignment_label_for).collect(),
            None => Vec::new(),
        });
        let mut derived = self.derived.borrow_mut();
        derived.labels_key = Some(key);
        derived.labels = Arc::clone(&labels);
        labels
    }

    /// Chip text of worktree file `index`, from the cached labels.
    pub fn lane_label(&self, index: usize) -> Option<String> {
        self.lane_labels().get(index).cloned()
    }

    pub fn change_groups(&self) -> Vec<ChangeGroup> {
        let key = self.worktree_key();
        if self.derived.borrow().file_key == Some(key) {
            return self.derived.borrow().groups.clone();
        }
        let groups = match self.worktree.as_deref() {
            Some(worktree) => group_files(&worktree.files, &lane_order(self.workspace.as_deref())),
            None => Vec::new(),
        };
        let slots = match self.worktree.as_deref() {
            Some(worktree) => slots_from(&groups, &worktree.files),
            None => Vec::new(),
        };
        let mut derived = self.derived.borrow_mut();
        derived.group_rebuilds = derived.group_rebuilds.saturating_add(1);
        derived.file_key = Some(key);
        derived.flat = groups
            .iter()
            .flat_map(|group| group.files.iter().copied())
            .collect();
        derived.slots = Arc::new(slots);
        derived.groups = groups.clone();
        groups
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn group_rebuilds(&self) -> u64 {
        self.derived.borrow().group_rebuilds
    }

    pub fn flat_files(&self) -> Vec<usize> {
        let _ = self.change_groups();
        self.derived.borrow().flat.clone()
    }

    pub fn file_slots(&self) -> Arc<Vec<FileSlot>> {
        let _ = self.change_groups();
        Arc::clone(&self.derived.borrow().slots)
    }

    pub fn tree_rows(&self) -> Arc<Vec<TreeRow>> {
        let (data_gen, worktree) = self.worktree_key();
        let key = (data_gen, worktree, set_stamp(&self.dirs_collapsed));
        if self.derived.borrow().tree_key == Some(key) {
            return Arc::clone(&self.derived.borrow().tree_rows);
        }
        let rows = match self.worktree.as_deref() {
            Some(worktree) => visible_tree(&build_tree(&worktree.files), &self.dirs_collapsed),
            None => Vec::new(),
        };
        let mut derived = self.derived.borrow_mut();
        derived.tree_key = Some(key);
        derived.tree_rows = Arc::new(rows);
        Arc::clone(&derived.tree_rows)
    }

    /// Sorted path index for folder rows and exact-path lookups.
    fn folder_index(&self) -> Arc<FolderIndex> {
        let key = self.worktree_key();
        if self.derived.borrow().folder_key == Some(key) {
            return Arc::clone(&self.derived.borrow().folders);
        }
        let index = match self.worktree.as_deref() {
            Some(worktree) => FolderIndex::build(&worktree.files),
            None => FolderIndex::default(),
        };
        let mut derived = self.derived.borrow_mut();
        derived.folder_key = Some(key);
        derived.folders = Arc::new(index);
        Arc::clone(&derived.folders)
    }

    /// Index of the file whose path is exactly `path`.
    pub fn file_index_by_path(&self, path: &str) -> Option<usize> {
        self.folder_index().lookup(path)
    }

    /// Every file under `folder`, in worktree order. Event-time callers only;
    /// per-frame mark checks go through [`Store::folder_fully_marked`].
    pub fn folder_paths_vec(&self, folder: &str) -> Vec<String> {
        let index = self.folder_index();
        let mut files: Vec<usize> = index.files[index.range(folder).clone()].to_vec();
        files.sort_unstable();
        match self.worktree.as_deref() {
            Some(worktree) => files
                .into_iter()
                .filter_map(|file| worktree.files.get(file))
                .map(|file| file.path.clone())
                .collect(),
            None => Vec::new(),
        }
    }

    /// Whether every file under `folder` is marked. O(files in the folder),
    /// no per-path allocation.
    pub fn folder_fully_marked(&self, folder: &str) -> bool {
        let index = self.folder_index();
        let range = index.range(folder);
        if range.is_empty() {
            return false;
        }
        index.paths[range]
            .iter()
            .all(|path| self.marked.contains(path))
    }

    pub fn toggle_dir(&mut self, path: &str) {
        let keep = self.uncommitted_path().unwrap_or_else(|| path.to_string());
        if !self.dirs_collapsed.remove(path) {
            self.dirs_collapsed.insert(path.to_string());
        }
        self.place_uncommitted(&keep);
    }

    pub fn lane_rows(&self) -> Arc<Vec<LaneItem>> {
        let (data_gen, workspace) = self.workspace_key();
        let key = (data_gen, workspace, set_stamp(&self.collapsed));
        if self.derived.borrow().lane_key == Some(key) {
            return Arc::clone(&self.derived.borrow().lane_rows);
        }
        let rows = match self.workspace.as_deref() {
            Some(workspace) => lane_items(workspace, &self.collapsed),
            None => Vec::new(),
        };
        let mut derived = self.derived.borrow_mut();
        derived.lane_rebuilds = derived.lane_rebuilds.saturating_add(1);
        derived.lane_key = Some(key);
        derived.lane_rows = Arc::new(rows);
        Arc::clone(&derived.lane_rows)
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn lane_rebuilds(&self) -> u64 {
        self.derived.borrow().lane_rebuilds
    }

    /// Indexes into `pages.branch_rows` for the current tab and the committed
    /// filter. Typing defers the filter to a debounce, so this stays cached
    /// while the text box updates.
    pub fn branch_indexes(&self) -> Arc<Vec<usize>> {
        let stamp = self.branch_stamp();
        if self.derived.borrow().branch_key == Some(stamp) {
            return Arc::clone(&self.derived.borrow().branch_indexes);
        }
        let indexes = Arc::new(self.visible_indices_fast());
        let mut derived = self.derived.borrow_mut();
        derived.branch_filter_rebuilds = derived.branch_filter_rebuilds.saturating_add(1);
        derived.branch_key = Some(stamp);
        derived.branch_indexes = Arc::clone(&indexes);
        indexes
    }

    /// Rows for the branches-page fallback, shared with `branch_indexes`.
    pub fn visible_branches(&self) -> Arc<Vec<BranchRow>> {
        let stamp = self.branch_stamp();
        if self.derived.borrow().branch_visible_key == Some(stamp) {
            return Arc::clone(&self.derived.borrow().branch_visible);
        }
        let indexes = self.branch_indexes();
        let rows: Arc<Vec<BranchRow>> = Arc::new(
            indexes
                .iter()
                .filter_map(|index| self.pages.branch_rows.get(*index).cloned())
                .collect(),
        );
        let mut derived = self.derived.borrow_mut();
        derived.branch_visible_key = Some(stamp);
        derived.branch_visible = Arc::clone(&rows);
        rows
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn branch_filter_rebuilds(&self) -> u64 {
        self.derived.borrow().branch_filter_rebuilds
    }

    /// Reveal another page of an already-open diff. The reload runs off the UI thread.
    pub fn show_more_diff(&mut self) -> Effect {
        self.diff_limit = self
            .diff_limit
            .saturating_add(gb_bridge::DIFF_ROW_CAP.max(DIFF_PAGE));
        self.reload_shown()
    }

    pub fn toggle_branch_collapsed(&mut self, index: usize) {
        let Some(item) = self.lane_rows().get(index).copied() else {
            return;
        };
        if item.commit.is_some() {
            return;
        }
        let Some(name) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.stacks.get(item.stack))
            .and_then(|stack| stack.branches.get(item.branch))
            .map(|branch| branch.name.clone())
        else {
            return;
        };
        let key = collapse_key(item.stack, &name);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.clamp();
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn commit_sha_at(&self, index: usize) -> Option<String> {
        self.lane_rows()
            .iter()
            .filter_map(|item| {
                let commit_index = item.commit?;
                self.commit_at(item.stack, item.branch, commit_index)
                    .map(|commit| commit.sha.clone())
            })
            .nth(index)
    }

    #[cfg(feature = "devtools")]
    pub fn focus_pane(&mut self, pane: Pane) -> bool {
        if pane == Pane::Detail && !self.detail_open() {
            return false;
        }
        self.pane = pane;
        true
    }

    /// Point the uncommitted list at `path` and open its worktree diff.
    #[cfg(any(test, feature = "devtools"))]
    pub fn focus_file(&mut self, path: &str) -> Option<DiffRequest> {
        let exists = self
            .worktree
            .as_ref()?
            .files
            .iter()
            .any(|file| file.path == path);
        if !exists {
            return None;
        }
        if self.view == ViewMode::Tree {
            self.reveal_ancestors(path);
        }
        self.place_uncommitted(path);
        match self.begin_diff(DiffRequest::Worktree {
            path: path.to_string(),
        }) {
            Effect::ShowDiff(request) => Some(request),
            _ => None,
        }
    }

    /// Read the open diff again without clearing the rows already on screen.
    pub fn reload_shown(&mut self) -> Effect {
        let Some(request) = self.shown_request() else {
            return Effect::None;
        };
        self.diff_loading = true;
        self.diff_error = None;
        Effect::ShowDiff(request)
    }

    pub fn focus_commit(&mut self, sha: &str) -> Effect {
        let resolved = self.resolve_sha(sha);
        if let Some(position) = self.lane_rows().iter().position(|item| {
            let Some(commit_index) = item.commit else {
                return false;
            };
            self.commit_at(item.stack, item.branch, commit_index)
                .is_some_and(|commit| {
                    commit.sha == resolved || commit.sha == sha || commit.short_sha == sha
                })
        }) {
            self.cursors[Pane::Branches.index()] = position;
        }
        let same = self
            .commit
            .as_ref()
            .is_some_and(|commit| commit.sha == resolved || commit.short_sha == resolved);
        if !same {
            self.commit = None;
        }
        self.side = SideFocus::Commit {
            sha: resolved.clone(),
        };
        self.commit_loading = true;
        self.commit_error = None;
        self.cursors[Pane::Detail.index()] = 0;
        self.pane = Pane::Detail;
        Effect::ShowCommit(resolved)
    }

    pub fn focus_branch(&mut self, name: &str) -> bool {
        let Some((stack_index, branch_index)) = self.find_branch(name) else {
            return false;
        };
        if let Some(position) = self.lane_rows().iter().position(|item| {
            item.commit.is_none() && item.stack == stack_index && item.branch == branch_index
        }) {
            self.cursors[Pane::Branches.index()] = position;
        }
        self.side = SideFocus::Branch {
            stack: stack_index,
            branch: branch_index,
        };
        self.commit_loading = false;
        self.commit_error = None;
        self.cursors[Pane::Detail.index()] = 0;
        self.pane = Pane::Detail;
        true
    }

    pub fn branch_info(&self) -> Option<BranchFocusInfo> {
        let SideFocus::Branch { stack, branch } = &self.side else {
            return None;
        };
        let (data_gen, workspace) = self.workspace_key();
        let key = (data_gen, workspace, *stack, *branch);
        if self.derived.borrow().info_key == Some(key) {
            return self.derived.borrow().info.clone();
        }
        let summary = self
            .workspace
            .as_ref()?
            .stacks
            .get(*stack)?
            .branches
            .get(*branch)?;
        let mut seen = HashSet::new();
        let mut authors = Vec::new();
        let mut earliest = None;
        let mut latest = None;
        for commit in &summary.commits {
            if seen.insert(commit.author.as_str()) {
                authors.push(commit.author.clone());
            }
            earliest =
                Some(earliest.map_or(commit.authored_at, |at: i64| at.min(commit.authored_at)));
            latest = Some(latest.map_or(commit.authored_at, |at: i64| at.max(commit.authored_at)));
        }
        let info = Some(BranchFocusInfo {
            name: summary.name.clone(),
            tip_sha: summary.tip_sha.clone(),
            tip_full: summary.tip_full.clone(),
            stack_label: format!("Stack {}", stack + 1),
            commit_count: summary.commits.len(),
            push_state: push_state(&summary.commits),
            authors,
            earliest,
            latest,
            commits: Arc::new(
                summary
                    .commits
                    .iter()
                    .map(|commit| CommitLine {
                        sha: commit.sha.clone(),
                        short_sha: commit.short_sha.clone(),
                        title: commit.title.clone(),
                    })
                    .collect(),
            ),
        });
        let mut derived = self.derived.borrow_mut();
        derived.info_key = Some(key);
        derived.info = info.clone();
        info
    }

    pub fn set_commit(&mut self, sha: &str, result: Result<CommitDetails, String>) {
        if !matches!(&self.side, SideFocus::Commit { sha: current } if current == sha) {
            return;
        }
        self.commit_loading = false;
        match result {
            Ok(details) => {
                self.commit = Some(details);
                self.commit_error = None;
                self.activity
                    .succeed("commit_details", "The commit is ready.");
            }
            Err(message) => {
                self.activity.fail("commit_details", message.clone());
                self.commit_error = Some(message);
            }
        }
        self.clamp();
    }

    pub fn set_diff(&mut self, request: &DiffRequest, result: Result<Vec<DiffRow>, String>) {
        let Some(current) = self.shown_request() else {
            return;
        };
        if &current != request {
            return;
        }
        self.diff_loading = false;
        self.hunk_cursor = 0;
        self.cursors[Pane::Changes.index()] = 0;
        match result {
            Ok(rows) => {
                self.diff_rows = Arc::new(rows);
                self.diff_error = None;
                self.activity.succeed("file_diff", "The diff is ready.");
            }
            Err(message) => {
                self.diff_rows = Arc::new(Vec::new());
                self.activity.fail("file_diff", message.clone());
                self.diff_error = Some(message);
            }
        }
        self.clamp();
    }

    /// Which registry key a follow-up effect starts. `None` is not a bridge call.
    pub fn effect_activity(effect: &Effect) -> Option<(&'static str, Option<String>)> {
        match effect {
            Effect::None => None,
            Effect::Refresh => Some(("refresh_status", None)),
            Effect::SelectProject(_) => Some(("open_project", None)),
            Effect::ShowCommit(_) => Some(("commit_details", None)),
            Effect::ShowDiff(_) => Some(("file_diff", None)),
            Effect::Mutate(action) => Some(mutate::mutation_activity(action)),
            Effect::Remote(op) => Some(remote::remote_activity(op)),
            Effect::Page(op) => Some(pages::page_activity(op)),
            Effect::Forge(ForgeOp::Publish { branch }) => {
                Some(("publish_draft", Some(branch.clone())))
            }
            Effect::Suggest(_) => Some(("suggest_message", None)),
            Effect::Settings(op) => Some(settings::settings_activity(op)),
        }
    }

    pub fn lanes_for(&self, request: &DiffRequest) -> Vec<Option<String>> {
        let DiffRequest::Worktree { path } = request else {
            return Vec::new();
        };
        self.worktree
            .as_ref()
            .and_then(|worktree| worktree.files.iter().find(|file| &file.path == path))
            .map(|file| file.hunks.iter().map(|hunk| hunk.lane.clone()).collect())
            .unwrap_or_default()
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn selected_item(&self) -> Option<String> {
        match self.pane {
            Pane::Uncommitted => self.uncommitted_path(),
            Pane::Changes => self.shown_path(),
            Pane::Branches => self.lane_label_at(self.cursor(Pane::Branches)),
            Pane::Detail => match &self.side {
                SideFocus::Commit { sha } => self
                    .commit
                    .as_ref()
                    .filter(|commit| commit.sha == *sha || commit.short_sha == *sha)
                    .and_then(|commit| commit.files.get(self.cursor(Pane::Detail)))
                    .map(|file| file.path.clone())
                    .or_else(|| Some(sha.clone())),
                SideFocus::Branch { stack, branch } => self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.stacks.get(*stack))
                    .and_then(|item| item.branches.get(*branch))
                    .and_then(|item| {
                        item.commits
                            .get(self.cursor(Pane::Detail))
                            .map(|commit| commit.sha.clone())
                            .or_else(|| Some(item.name.clone()))
                    }),
                SideFocus::None => None,
            },
        }
    }

    pub fn shown_path(&self) -> Option<String> {
        match &self.shown {
            ShownFile::None => None,
            ShownFile::Worktree { path } | ShownFile::Commit { path, .. } => Some(path.clone()),
        }
    }

    /// Header for the Changes column, including the lane or branch when known.
    pub fn changes_heading(&self) -> Option<String> {
        match &self.shown {
            ShownFile::None => None,
            ShownFile::Worktree { path } => {
                let lane = self
                    .file_index_by_path(path)
                    .and_then(|index| self.lane_label(index));
                Some(match lane {
                    Some(lane) if lane != "Unassigned" => format!("Uncommitted · {lane} · {path}"),
                    _ => format!("Uncommitted · {path}"),
                })
            }
            ShownFile::Commit { sha, path } => {
                let short = short7(sha);
                Some(match self.branch_name_for(sha) {
                    Some(name) => format!("Commit {short} · {name} · {path}"),
                    None => format!("Commit {short} · {path}"),
                })
            }
        }
    }

    /// "Needs setup" or "Open in GitButler first" when this project has shown that.
    pub fn project_hint(&self, id: &str) -> Option<String> {
        if self.selected.as_deref() == Some(id)
            && let Some(hint) = self.live_hint()
        {
            return Some(hint);
        }
        self.project_hints.get(id).cloned()
    }

    pub fn blocks_open(&self) -> bool {
        !matches!(
            self.env.as_ref().map(|env| &env.compat),
            Some(Compat::Match)
        )
    }

    /// Banner shown until the installed app matches the pinned build.
    pub fn banner(&self) -> Option<String> {
        if self.error.as_deref() == Some(gb_bridge::WRONG_CHANNEL_MESSAGE) {
            return Some(gb_bridge::WRONG_CHANNEL_MESSAGE.to_string());
        }
        let env = self.env.as_ref()?;
        match &env.compat {
            Compat::Match => None,
            Compat::Mismatch { installed, pinned } => Some(format!(
                "GitButler {installed} is installed; gb-gpui is built for {pinned}. Projects are not opened until versions match."
            )),
            Compat::NotInstalled => Some(format!(
                "GitButler is not installed; gb-gpui is built for {}. Projects are not opened until versions match.",
                env.pinned
            )),
        }
    }

    /// Fixed status strip. Empty when nothing needs saying, so the row stays put.
    pub fn strip_text(&self) -> String {
        if let Some(banner) = self.banner() {
            return banner;
        }
        if self.exclusive == Some(false) {
            return "Another GitButler window has this project. Watching continues without the project lock.".to_string();
        }
        if let Some(line) = self.activity.strip_line() {
            return line;
        }
        if let Some(error) = self
            .error
            .as_ref()
            .or(self.commit_error.as_ref())
            .or(self.diff_error.as_ref())
        {
            return error.clone();
        }
        if let Some(note) = self.awareness() {
            return note;
        }
        self.remote.status_line()
    }

    /// A fixed sentence when the project is quiet but the user should know the next step.
    pub fn awareness(&self) -> Option<String> {
        if self.activity.strip_line().is_some() {
            return None;
        }
        if self.needs_setup() {
            return Some("No remote is set. Choose a target branch to fetch and push.".to_string());
        }
        if self.remote.fetch_error.as_ref().is_some_and(|message| {
            let lower = message.to_ascii_lowercase();
            lower.contains("offline")
                || lower.contains("reach the remote")
                || lower.contains("network")
        }) {
            return self.remote.fetch_error.clone();
        }
        if self.workspace.is_some()
            && !self.loading
            && !self.refreshing
            && self
                .worktree
                .as_ref()
                .is_some_and(|worktree| worktree.files.is_empty())
        {
            return Some(
                "Nothing to commit. Change a file in the worktree and it will show up here."
                    .to_string(),
            );
        }
        None
    }

    fn settle_refresh(&mut self, error: Option<String>) {
        self.activity
            .succeed("list_projects", "Projects are up to date.");
        self.activity
            .succeed("open_project", "The project is open.");
        self.activity.succeed("worktree", "Changes are up to date.");
        self.activity
            .succeed("forge_status", "Pull requests are up to date.");
        match error {
            Some(error) => self.activity.fail("refresh_status", error),
            None => self.activity.succeed("refresh_status", "Refresh finished."),
        }
    }

    /// Run `op` again after Retry. Unknown or unsafe ops do nothing.
    pub fn retry_effect(&mut self, op: &str) -> Effect {
        let row = self.activity.row(op);
        match op {
            "fetch" => self.request_fetch(),
            "push" => {
                let Some(lane) = row else {
                    return Effect::None;
                };
                self.push_now(&lane, gb_bridge::ForcePush::No)
                    .unwrap_or(Effect::None)
            }
            "refresh_status" | "list_projects" | "open_project" | "worktree" | "forge_status" => {
                Effect::Refresh
            }
            "list_branches" | "remote_branches" => {
                self.pages.branches_loading = true;
                self.pages.branches_error = None;
                Effect::Page(PageOp::ListBranches)
            }
            "list_snapshots" | "snapshots" => {
                self.pages.history_loading = true;
                self.pages.history_error = None;
                Effect::Page(PageOp::ListSnapshots)
            }
            "file_diff" => self.reload_shown(),
            "commit_details" => match &self.side {
                SideFocus::Commit { sha } => Effect::ShowCommit(sha.clone()),
                _ => Effect::None,
            },
            "suggest_message" => self.request_suggestion(),
            "integrate_preview" => self.open_integrate(),
            "integrate" => self.confirm_integrate(),
            "read_global" | "read_project" => {
                self.settings.open = true;
                self.settings.loading = true;
                self.settings.error = None;
                Effect::Settings(SettingsOp::Load)
            }
            "publish_draft" => {
                let Some(branch) = row else {
                    return Effect::None;
                };
                self.request_publish(&branch).unwrap_or(Effect::None)
            }
            "apply_branch" => {
                let Some(name) = row else {
                    return Effect::None;
                };
                self.apply_named(&name).unwrap_or(Effect::None)
            }
            "unapply_branch" => {
                let Some(name) = row else {
                    return Effect::None;
                };
                self.unapply_named(Some(&name)).unwrap_or(Effect::None)
            }
            "add_project" => Effect::None,
            "set_target" => Effect::None,
            "restore_snapshot" => Effect::None,
            _ => Effect::None,
        }
    }

    fn clear_focus(&mut self) {
        self.side = SideFocus::None;
        self.shown = ShownFile::None;
        self.commit_loading = false;
        self.diff_loading = false;
        self.commit_error = None;
        self.diff_error = None;
        self.commit = None;
        self.diff_rows = Arc::new(Vec::new());
        self.hunk_cursor = 0;
    }

    fn begin_diff(&mut self, request: DiffRequest) -> Effect {
        self.shown = match &request {
            DiffRequest::Worktree { path } => ShownFile::Worktree { path: path.clone() },
            DiffRequest::Commit { sha, path } => ShownFile::Commit {
                sha: sha.clone(),
                path: path.clone(),
            },
        };
        self.diff_loading = true;
        self.diff_error = None;
        self.diff_rows = Arc::new(Vec::new());
        self.hunk_cursor = 0;
        self.cursors[Pane::Changes.index()] = 0;
        self.pane = Pane::Changes;
        Effect::ShowDiff(request)
    }

    fn shown_request(&self) -> Option<DiffRequest> {
        match &self.shown {
            ShownFile::None => None,
            ShownFile::Worktree { path } => Some(DiffRequest::Worktree { path: path.clone() }),
            ShownFile::Commit { sha, path } => Some(DiffRequest::Commit {
                sha: sha.clone(),
                path: path.clone(),
            }),
        }
    }

    fn open_uncommitted(&mut self) -> Effect {
        if self.view == ViewMode::Tree
            && let Some(row) = self
                .tree_rows()
                .get(self.cursor(Pane::Uncommitted))
                .cloned()
            && row.file.is_none()
        {
            self.toggle_dir(&row.path);
            return Effect::None;
        }
        let Some(path) = self.uncommitted_path() else {
            return Effect::None;
        };
        self.begin_diff(DiffRequest::Worktree { path })
    }

    fn open_lane_row(&mut self) -> Effect {
        let Some(item) = self.lane_rows().get(self.cursor(Pane::Branches)).copied() else {
            return Effect::None;
        };
        let (name, sha) = {
            let Some(branch) = self
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.stacks.get(item.stack))
                .and_then(|stack| stack.branches.get(item.branch))
            else {
                return Effect::None;
            };
            let sha = item
                .commit
                .and_then(|index| branch.commits.get(index))
                .map(|commit| commit.sha.clone());
            (branch.name.clone(), sha)
        };
        if let Some(sha) = sha {
            self.focus_commit(&sha)
        } else {
            self.focus_branch(&name);
            Effect::None
        }
    }

    fn open_detail(&mut self) -> Effect {
        match self.side.clone() {
            SideFocus::Commit { sha } => {
                let Some(commit) = self.commit.clone() else {
                    return Effect::None;
                };
                if commit.sha != sha && commit.short_sha != sha {
                    return Effect::None;
                }
                let Some(file) = commit.files.get(self.cursor(Pane::Detail)) else {
                    return Effect::None;
                };
                self.begin_diff(DiffRequest::Commit {
                    sha,
                    path: file.path.clone(),
                })
            }
            SideFocus::Branch { stack, branch } => {
                let Some(sha) = self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.stacks.get(stack))
                    .and_then(|item| item.branches.get(branch))
                    .and_then(|item| item.commits.get(self.cursor(Pane::Detail)))
                    .map(|commit| commit.sha.clone())
                else {
                    return Effect::None;
                };
                self.focus_commit(&sha)
            }
            SideFocus::None => Effect::None,
        }
    }

    fn go_back(&mut self) -> Effect {
        if self.help {
            self.help = false;
            return Effect::None;
        }
        if self.project_menu {
            self.project_menu = false;
            return Effect::None;
        }
        if self.detail_open() {
            self.side = SideFocus::None;
            self.commit_loading = false;
            self.commit_error = None;
            if self.pane == Pane::Detail {
                self.pane = Pane::Branches;
            }
            return Effect::None;
        }
        if !matches!(self.shown, ShownFile::None) {
            self.shown = ShownFile::None;
            self.diff_loading = false;
            self.diff_error = None;
            self.diff_rows = Arc::new(Vec::new());
            self.hunk_cursor = 0;
        }
        Effect::None
    }

    fn choose_project(&mut self) -> Effect {
        if self.project_cursor == self.projects.len() {
            if self.blocks_open() {
                return Effect::None;
            }
            self.open_add_field();
            return Effect::None;
        }
        if self.blocks_open() {
            return Effect::None;
        }
        let Some(project) = self.projects.get(self.project_cursor) else {
            return Effect::None;
        };
        let id = project.id.clone();
        self.project_menu = false;
        Effect::SelectProject(id)
    }

    fn move_project(&mut self, delta: i32) {
        let len = self.project_menu_len();
        if len == 0 {
            return;
        }
        let next = self.project_cursor as i32 + delta;
        self.project_cursor = next.clamp(0, len as i32 - 1) as usize;
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.list_len(self.pane);
        if len == 0 {
            return;
        }
        let slot = self.pane.index();
        let next = self.cursors[slot] as i32 + delta;
        self.cursors[slot] = next.clamp(0, len as i32 - 1) as usize;
        if self.pane == Pane::Changes {
            self.scroll_to = Some(self.cursors[slot]);
        }
    }

    fn move_hunk(&mut self, delta: i32) {
        let count = self
            .diff_rows
            .iter()
            .filter(|row| row.kind == RowKind::Hunk)
            .count();
        if count == 0 {
            return;
        }
        let next = self.hunk_cursor as i32 + delta;
        self.hunk_cursor = next.clamp(0, count as i32 - 1) as usize;
        self.scroll_to = self.hunk_row();
    }

    fn diff_focused(&self) -> bool {
        self.pane == Pane::Changes && !matches!(self.shown, ShownFile::None)
    }

    fn cycle(&mut self, step: i32) {
        let panes = self.panes();
        let position = panes
            .iter()
            .position(|pane| *pane == self.pane)
            .unwrap_or(0);
        let next = (position as i32 + step).rem_euclid(panes.len() as i32) as usize;
        self.pane = panes[next];
    }

    fn list_len(&self, pane: Pane) -> usize {
        match pane {
            Pane::Uncommitted => match self.view {
                ViewMode::List => self.flat_files().len(),
                ViewMode::Tree => self.tree_rows().len(),
            },
            Pane::Changes => {
                if matches!(self.shown, ShownFile::None) {
                    0
                } else {
                    self.diff_rows.len()
                }
            }
            Pane::Branches => self.lane_rows().len(),
            Pane::Detail => match &self.side {
                SideFocus::Commit { sha } => self
                    .commit
                    .as_ref()
                    .filter(|commit| commit.sha == *sha || commit.short_sha == *sha)
                    .map(|commit| commit.files.len())
                    .unwrap_or(0),
                SideFocus::Branch { stack, branch } => self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.stacks.get(*stack))
                    .and_then(|item| item.branches.get(*branch))
                    .map(|item| item.commits.len())
                    .unwrap_or(0),
                SideFocus::None => 0,
            },
        }
    }

    fn clamp(&mut self) {
        self.clamp_shown();
        self.clamp_side();
        self.clamp_project_cursor();
        for pane in [
            Pane::Uncommitted,
            Pane::Changes,
            Pane::Branches,
            Pane::Detail,
        ] {
            let len = self.list_len(pane);
            let slot = pane.index();
            if len == 0 {
                self.cursors[slot] = 0;
            } else if self.cursors[slot] >= len {
                self.cursors[slot] = len - 1;
            }
        }
    }

    fn project_menu_len(&self) -> usize {
        self.projects.len() + 1
    }

    fn clamp_project_cursor(&mut self) {
        let len = self.project_menu_len();
        if self.project_cursor >= len {
            self.project_cursor = len.saturating_sub(1);
        }
    }

    fn clamp_side(&mut self) {
        if let SideFocus::Branch { stack, branch } = &self.side {
            let ok = self
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.stacks.get(*stack))
                .and_then(|item| item.branches.get(*branch))
                .is_some();
            if !ok {
                self.side = SideFocus::None;
            }
        }
        if let SideFocus::Commit { sha } = &self.side {
            let sha = sha.clone();
            let alive = self.workspace.as_ref().is_some_and(|workspace| {
                workspace.stacks.iter().any(|stack| {
                    stack.branches.iter().any(|branch| {
                        branch
                            .commits
                            .iter()
                            .any(|commit| commit.sha == sha || commit.short_sha == sha)
                    })
                })
            });
            if !alive {
                self.side = SideFocus::None;
                self.commit = None;
                self.commit_loading = false;
                self.commit_error = None;
            }
        }
        if self.pane == Pane::Detail && !self.detail_open() {
            self.pane = Pane::Branches;
        }
    }

    fn clamp_shown(&mut self) {
        let ShownFile::Worktree { path } = &self.shown else {
            return;
        };
        let gone = self
            .worktree
            .as_ref()
            .is_none_or(|worktree| !worktree.files.iter().any(|file| file.path == *path));
        if gone {
            self.shown = ShownFile::None;
            self.diff_rows = Arc::new(Vec::new());
            self.diff_loading = false;
        }
    }

    fn uncommitted_path(&self) -> Option<String> {
        let index = self.uncommitted_file_index()?;
        self.worktree
            .as_ref()?
            .files
            .get(index)
            .map(|file| file.path.clone())
    }

    fn uncommitted_file_index(&self) -> Option<usize> {
        match self.view {
            ViewMode::List => self
                .flat_files()
                .get(self.cursor(Pane::Uncommitted))
                .copied(),
            ViewMode::Tree => self
                .tree_rows()
                .get(self.cursor(Pane::Uncommitted))
                .and_then(|row| row.file),
        }
    }

    fn place_uncommitted(&mut self, path: &str) {
        let position = match self.view {
            ViewMode::List => self.flat_files().into_iter().position(|index| {
                self.worktree
                    .as_ref()
                    .and_then(|worktree| worktree.files.get(index))
                    .is_some_and(|file| file.path == path)
            }),
            ViewMode::Tree => {
                let rows = self.tree_rows();
                let mut best: Option<(usize, usize)> = None;
                for (index, row) in rows.iter().enumerate() {
                    let hit = row.path == path || path.starts_with(&format!("{}/", row.path));
                    if hit && best.is_none_or(|(_, len)| row.path.len() > len) {
                        best = Some((index, row.path.len()));
                    }
                }
                best.map(|(index, _)| index)
            }
        };
        if let Some(position) = position {
            self.cursors[Pane::Uncommitted.index()] = position;
        }
        self.clamp();
    }

    fn reveal_ancestors(&mut self, path: &str) {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if parts.len() <= 1 {
            return;
        }
        let mut acc = String::new();
        for part in &parts[..parts.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            self.dirs_collapsed.remove(&acc);
        }
    }

    fn find_branch(&self, name: &str) -> Option<(usize, usize)> {
        let workspace = self.workspace.as_ref()?;
        for (stack_index, stack) in workspace.stacks.iter().enumerate() {
            for (branch_index, branch) in stack.branches.iter().enumerate() {
                if branch.name == name {
                    return Some((stack_index, branch_index));
                }
            }
        }
        None
    }

    fn resolve_sha(&self, sha: &str) -> String {
        self.workspace
            .as_ref()
            .and_then(|workspace| {
                workspace.stacks.iter().find_map(|stack| {
                    stack.branches.iter().find_map(|branch| {
                        branch.commits.iter().find_map(|commit| {
                            (commit.sha == sha || commit.short_sha == sha)
                                .then(|| commit.sha.clone())
                        })
                    })
                })
            })
            .unwrap_or_else(|| sha.to_string())
    }

    fn commit_at(&self, stack: usize, branch: usize, commit: usize) -> Option<&CommitSummary> {
        self.workspace
            .as_ref()?
            .stacks
            .get(stack)?
            .branches
            .get(branch)?
            .commits
            .get(commit)
    }

    #[cfg(any(test, feature = "devtools"))]
    fn lane_label_at(&self, index: usize) -> Option<String> {
        let item = self.lane_rows().get(index).copied()?;
        let branch = self
            .workspace
            .as_ref()?
            .stacks
            .get(item.stack)?
            .branches
            .get(item.branch)?;
        if let Some(commit_index) = item.commit {
            Some(branch.commits.get(commit_index)?.sha.clone())
        } else {
            Some(branch.name.clone())
        }
    }

    fn branch_name_for(&self, sha: &str) -> Option<String> {
        let (data_gen, workspace) = self.workspace_key();
        let key = (data_gen, workspace, sha.to_string());
        if self.derived.borrow().branch_name_key.as_ref() == Some(&key)
            && let Some(cached) = self.derived.borrow().branch_name.clone()
        {
            return cached;
        }
        let found = self.branch_name_scan(sha);
        let mut derived = self.derived.borrow_mut();
        derived.branch_name_key = Some(key);
        derived.branch_name = Some(found.clone());
        found
    }

    /// The uncached scan `branch_name_for` memoizes.
    fn branch_name_scan(&self, sha: &str) -> Option<String> {
        let workspace = self.workspace.as_ref()?;
        for stack in &workspace.stacks {
            for branch in &stack.branches {
                let hit = branch.tip_sha == sha
                    || branch.tip_full == sha
                    || branch
                        .commits
                        .iter()
                        .any(|commit| commit.sha == sha || commit.short_sha == sha);
                if hit {
                    return Some(branch.name.clone());
                }
            }
        }
        None
    }

    fn sync_hints(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if let Some(hint) = self.live_hint() {
            self.project_hints.insert(id, hint);
        } else if self.error.is_none() {
            self.project_hints.remove(&id);
        }
    }

    fn live_hint(&self) -> Option<String> {
        if let Some(error) = &self.error {
            let lower = error.to_ascii_lowercase();
            if lower.contains("gitbutler first") {
                return Some("Open in GitButler first".to_string());
            }
            if lower.contains("needs setup")
                || lower.contains("no target")
                || lower.contains("target branch")
            {
                return Some("Needs setup".to_string());
            }
        }
        if self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.target_branch.is_none())
        {
            return Some("Needs setup".to_string());
        }
        None
    }
}

pub fn collapse_key(stack: usize, name: &str) -> String {
    format!("{stack}/{name}")
}

/// The text `FileChange::assignment_label` produces, without borrowing the
/// hunk list twice. The cached lane labels must match it exactly.
fn assignment_label_for(file: &FileChange) -> String {
    let mut lanes: Vec<&str> = Vec::new();
    for hunk in &file.hunks {
        if let Some(lane) = hunk.lane.as_deref()
            && !lanes.contains(&lane)
        {
            lanes.push(lane);
        }
    }
    match lanes.as_slice() {
        [] => "Unassigned".to_string(),
        [lane] if file.hunks.len() <= 1 => (*lane).to_string(),
        [lane] => format!("{lane} · {} hunks", file.hunks.len()),
        _ => "split".to_string(),
    }
}

fn short7(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn push_state(commits: &[CommitSummary]) -> Option<String> {
    if commits.is_empty() {
        return None;
    }
    let pushed = commits.iter().filter(|commit| commit.pushed).count();
    Some(match pushed {
        0 => "Not pushed".to_string(),
        count if count == commits.len() => "Pushed".to_string(),
        _ => "Partly pushed".to_string(),
    })
}

fn arc_key<T>(value: Option<&Arc<T>>) -> usize {
    value.map(|arc| Arc::as_ptr(arc) as usize).unwrap_or(0)
}

fn set_stamp(set: &BTreeSet<String>) -> u64 {
    fn mix(hash: u64, bytes: &[u8]) -> u64 {
        let mut hash = hash;
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^ 0xff
    }
    let mut hash = 0xcbf29ce484222325u64 ^ set.len() as u64;
    for item in set {
        hash = mix(hash, item.as_bytes());
    }
    hash
}

fn branch_stamp(pages: &Pages, filter: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in filter.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= pages.branch_rows.len() as u64;
    hash ^= pages.branch_rows.as_ptr() as u64;
    hash ^= match pages.tab {
        pages::BranchTab::Local => 1,
        pages::BranchTab::Remote => 2,
        pages::BranchTab::PullRequests => 3,
    };
    hash
}

fn slots_from(groups: &[ChangeGroup], files: &[FileChange]) -> Vec<FileSlot> {
    let mut slots = Vec::new();
    let mut flat = 0usize;
    for group in groups {
        let paths: Vec<String> = group
            .files
            .iter()
            .filter_map(|index| files.get(*index).map(|file| file.path.clone()))
            .collect();
        slots.push(FileSlot::Group {
            name: group.name.clone(),
            files: group.files.clone(),
            paths: paths.into(),
        });
        for index in &group.files {
            slots.push(FileSlot::File {
                flat,
                index: *index,
            });
            flat += 1;
        }
    }
    slots
}

fn lane_items(workspace: &WorkspaceSummary, collapsed: &BTreeSet<String>) -> Vec<LaneItem> {
    let mut rows = Vec::new();
    for (stack_index, stack) in workspace.stacks.iter().enumerate() {
        for (branch_index, branch) in stack.branches.iter().enumerate() {
            rows.push(LaneItem {
                stack: stack_index,
                branch: branch_index,
                commit: None,
            });
            if collapsed.contains(&collapse_key(stack_index, &branch.name)) {
                continue;
            }
            for commit_index in 0..branch.commits.len() {
                rows.push(LaneItem {
                    stack: stack_index,
                    branch: branch_index,
                    commit: Some(commit_index),
                });
            }
        }
    }
    rows
}

pub(crate) fn lane_order(workspace: Option<&WorkspaceSummary>) -> Vec<String> {
    let Some(workspace) = workspace else {
        return Vec::new();
    };
    workspace
        .stacks
        .iter()
        .flat_map(|stack| stack.names().map(str::to_string))
        .collect()
}

pub(crate) fn group_files(files: &[FileChange], lane_order: &[String]) -> Vec<ChangeGroup> {
    let mut names: Vec<String> = lane_order.to_vec();
    for extra in ["Split", "Unassigned"] {
        if !names.iter().any(|name| name == extra) {
            names.push(extra.to_string());
        }
    }
    let grouped: Vec<String> = files.iter().map(FileChange::group_name).collect();
    for group in &grouped {
        if !names.iter().any(|name| name == group) {
            names.push(group.clone());
        }
    }
    names
        .into_iter()
        .filter_map(|name| {
            let files = grouped
                .iter()
                .enumerate()
                .filter(|(_, group)| *group == &name)
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if files.is_empty() {
                None
            } else {
                Some(ChangeGroup { name, files })
            }
        })
        .collect()
}

/// Nest `files` by path. Directories sort before files, then by name.
/// The same paths always produce the same shape, whatever the input order.
pub fn build_tree(files: &[FileChange]) -> Vec<TreeNode> {
    let mut roots = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let parts: Vec<&str> = file
            .path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        insert(
            &mut roots,
            "",
            &parts,
            index,
            file.additions,
            file.deletions,
        );
    }
    finish(&mut roots);
    roots
}

fn insert(
    nodes: &mut Vec<TreeNode>,
    prefix: &str,
    parts: &[&str],
    file_index: usize,
    additions: u32,
    deletions: u32,
) {
    if parts.is_empty() {
        return;
    }
    let name = parts[0];
    let path = if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    };
    if parts.len() == 1 {
        if let Some(node) = nodes.iter_mut().find(|node| node.name == name) {
            node.path = path;
            node.file = Some(file_index);
            node.additions = additions;
            node.deletions = deletions;
            node.file_count = 1;
            return;
        }
        nodes.push(TreeNode {
            name: name.to_string(),
            path,
            file: Some(file_index),
            additions,
            deletions,
            file_count: 1,
            children: Vec::new(),
        });
        return;
    }
    if let Some(position) = nodes
        .iter()
        .position(|node| node.name == name && node.file.is_none())
    {
        let prefix = nodes[position].path.clone();
        insert(
            &mut nodes[position].children,
            &prefix,
            &parts[1..],
            file_index,
            additions,
            deletions,
        );
        return;
    }
    let mut dir = TreeNode {
        name: name.to_string(),
        path: path.clone(),
        file: None,
        additions: 0,
        deletions: 0,
        file_count: 0,
        children: Vec::new(),
    };
    insert(
        &mut dir.children,
        &path,
        &parts[1..],
        file_index,
        additions,
        deletions,
    );
    nodes.push(dir);
}

fn finish(nodes: &mut [TreeNode]) {
    for node in nodes.iter_mut() {
        finish(&mut node.children);
        if node.file.is_none() {
            node.file_count = node.children.iter().map(|child| child.file_count).sum();
            node.additions = node.children.iter().map(|child| child.additions).sum();
            node.deletions = node.children.iter().map(|child| child.deletions).sum();
        } else {
            node.file_count = 1;
        }
    }
    nodes.sort_by(
        |left, right| match (left.file.is_none(), right.file.is_none()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => left.name.cmp(&right.name),
        },
    );
}

pub fn visible_tree(nodes: &[TreeNode], collapsed: &BTreeSet<String>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    walk_tree(nodes, 0, collapsed, &mut rows);
    rows
}

fn walk_tree(
    nodes: &[TreeNode],
    depth: usize,
    collapsed: &BTreeSet<String>,
    out: &mut Vec<TreeRow>,
) {
    for node in nodes {
        let directory = node.file.is_none();
        let expanded = directory && !collapsed.contains(&node.path);
        out.push(TreeRow {
            path: node.path.clone(),
            name: node.name.clone(),
            depth,
            file: node.file,
            additions: node.additions,
            deletions: node.deletions,
            file_count: node.file_count,
            expanded,
        });
        if expanded {
            walk_tree(&node.children, depth + 1, collapsed, out);
        }
    }
}

/// Read environment, the project list, the workspace, and uncommitted files.
/// A version mismatch stops before any project database is opened.
pub fn load(backend: &dyn Backend, selected: Option<&str>) -> Loaded {
    let env = match backend.environment() {
        Ok(env) => env,
        Err(error) => {
            let message = error.message;
            let list_reason =
                (message == gb_bridge::WRONG_CHANNEL_MESSAGE).then(|| message.clone());
            return Loaded {
                env: Environment {
                    installed: None,
                    pinned: gb_bridge::PINNED_VERSION.to_string(),
                    app_data_dir: Default::default(),
                    config_dir: Default::default(),
                    compat: Compat::NotInstalled,
                },
                projects: Vec::new(),
                list_reason,
                workspace: None,
                worktree: None,
                forge: ForgeStatus::disconnected(),
                error: Some(message),
            };
        }
    };
    if env.compat != Compat::Match {
        return Loaded {
            env,
            projects: Vec::new(),
            list_reason: None,
            workspace: None,
            worktree: None,
            forge: ForgeStatus::disconnected(),
            error: None,
        };
    }
    let (projects, list_reason, list_error) = match backend.list_projects() {
        Ok(list) => (list.projects, list.reason, None),
        Err(error) => (Vec::new(), None, Some(error.message)),
    };
    if list_error.is_some() {
        return Loaded {
            env,
            projects,
            list_reason,
            workspace: None,
            worktree: None,
            forge: ForgeStatus::disconnected(),
            error: list_error,
        };
    }
    let Some(id) = selected.filter(|id| projects.iter().any(|project| project.id == *id)) else {
        return Loaded {
            env,
            projects,
            list_reason,
            workspace: None,
            worktree: None,
            forge: ForgeStatus::disconnected(),
            error: None,
        };
    };
    let workspace = match backend.open_project(id) {
        Ok(workspace) => workspace,
        Err(error) => {
            return Loaded {
                env,
                projects,
                list_reason,
                workspace: None,
                worktree: None,
                forge: ForgeStatus::disconnected(),
                error: Some(error.message),
            };
        }
    };
    let (worktree, error) = match backend.worktree(id) {
        Ok(worktree) => (Some(worktree), None),
        Err(error) => (None, Some(error.message)),
    };
    let forge = backend
        .forge_status(id)
        .unwrap_or_else(|_| ForgeStatus::disconnected());
    Loaded {
        env,
        projects,
        list_reason,
        workspace: Some(workspace),
        worktree,
        forge,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gb_bridge::{
        AssignedHunk, BranchPlace, BranchRow, BranchSummary, CommitFile, CommitSummary,
        FakeBackend, FileStatus, StackSummary,
    };

    fn ready() -> (FakeBackend, Store) {
        let backend = FakeBackend::demo();
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        (backend, store)
    }

    fn one_commit(title: &str) -> CommitSummary {
        CommitSummary {
            sha: "abc1234abc1234abc1234abc1234abc1234abc12".to_string(),
            short_sha: "abc1234".to_string(),
            title: title.to_string(),
            author: "Ada <ada@example.com>".to_string(),
            authored_at: 0,
            conflicted: false,
            pushed: false,
        }
    }

    fn change(path: &str, additions: u32, deletions: u32) -> FileChange {
        FileChange {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            additions,
            deletions,
            hunks: Vec::new(),
        }
    }

    #[test]
    fn selecting_a_project_loads_its_stacks_and_files() {
        let (_backend, store) = ready();
        assert!(store.banner().is_none());
        assert_eq!(store.projects.len(), 1);
        let workspace = store.workspace.as_ref().expect("workspace");
        assert_eq!(workspace.stacks.len(), 3);
        assert!(store.worktree.as_ref().unwrap().files.len() >= 6);
        assert!(store.last_refresh.is_some());
        let labels: Vec<_> = store
            .worktree
            .as_ref()
            .unwrap()
            .files
            .iter()
            .map(|file| file.assignment_label())
            .collect();
        assert!(labels.iter().any(|label| label == "split"), "{labels:?}");
        assert_eq!(COLUMNS, 3);
        assert_eq!(store.pane, Pane::Uncommitted);
    }

    #[test]
    fn a_worktree_event_replaces_files_and_git_activity_marks_the_graph() {
        let (backend, mut store) = ready();
        let before = store.worktree.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watch = backend
            .watch(
                "demo-project",
                Box::new(move |event| {
                    let _ = tx.send(event);
                }),
            )
            .unwrap();
        backend.trigger_fake("demo-project", "worktree").unwrap();
        let event = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        store.apply_event(event);
        assert_eq!(store.change_count, 1);
        assert!(!store.workspace_stale);
        assert_eq!(
            store.worktree.as_ref().map(|view| view.files.len()),
            before.as_ref().map(|view| view.files.len())
        );

        backend.trigger_fake("demo-project", "git").unwrap();
        let event = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert!(event.refreshes_workspace());
        let files = store.worktree.clone();
        store.apply_event(event);
        assert!(store.workspace_stale);
        assert_eq!(store.change_count, 2);
        assert_eq!(store.worktree, files);
    }

    #[test]
    fn a_refresh_keeps_watcher_assignments() {
        let (_backend, mut store) = ready();
        let previous = store.worktree.clone().unwrap();
        assert!(
            previous
                .files
                .iter()
                .any(|file| file.assignment_label() == "split")
        );
        let mut stripped = (*previous).clone();
        for file in &mut stripped.files {
            file.hunks.clear();
        }
        store.apply(Loaded {
            env: store.env.clone().unwrap(),
            projects: store.projects.clone(),
            list_reason: None,
            workspace: store.workspace.as_deref().cloned(),
            worktree: Some(stripped),
            forge: ForgeStatus::disconnected(),
            error: None,
        });
        let split = store
            .worktree
            .as_ref()
            .unwrap()
            .files
            .iter()
            .find(|file| file.path == "src/split.rs")
            .unwrap();
        assert_eq!(split.assignment_label(), "split");
    }

    #[test]
    fn arrow_keys_move_the_cursor_and_enter_opens() {
        let (_backend, mut store) = ready();
        store.pane = Pane::Uncommitted;
        assert_eq!(store.apply_key("down"), Effect::None);
        assert_eq!(store.cursor(Pane::Uncommitted), 1);
        assert!(matches!(store.shown, ShownFile::None));
        let effect = store.apply_key("enter");
        assert!(matches!(
            effect,
            Effect::ShowDiff(DiffRequest::Worktree { .. })
        ));
        assert_eq!(store.pane, Pane::Changes);
    }

    #[test]
    fn tab_cycles_the_three_panes_and_escape_steps_back() {
        let (_backend, mut store) = ready();
        assert_eq!(store.panes().len(), 3);
        assert_eq!(store.pane, Pane::Uncommitted);
        store.apply_key("tab");
        assert_eq!(store.pane, Pane::Changes);
        store.apply_key("right");
        assert_eq!(store.pane, Pane::Branches);
        store.apply_key("shift-tab");
        assert_eq!(store.pane, Pane::Changes);
        store.apply_key("tab");
        store.apply_key("tab");
        assert_eq!(store.pane, Pane::Uncommitted);
        store.apply_key("enter");
        store.apply_key("escape");
        assert!(matches!(store.shown, ShownFile::None));
        store.help = true;
        store.apply_key("escape");
        assert!(!store.help);
    }

    #[test]
    fn j_and_k_move_hunks_only_in_the_diff() {
        let (_backend, mut store) = ready();
        store.pane = Pane::Uncommitted;
        assert_eq!(store.apply_key("j"), Effect::None);
        assert_eq!(store.cursor(Pane::Uncommitted), 0);
        store.apply_key("enter");
        store.set_diff(
            &DiffRequest::Worktree {
                path: store.selected_item().unwrap(),
            },
            Ok(vec![
                DiffRow {
                    kind: RowKind::Hunk,
                    old_line: None,
                    new_line: None,
                    text: "@@ -1 +1 @@".to_string(),
                    hunk: Some(0),
                    lane: Some("lane-a".to_string()),
                },
                DiffRow {
                    kind: RowKind::Add,
                    old_line: None,
                    new_line: Some(1),
                    text: "line".to_string(),
                    hunk: Some(0),
                    lane: Some("lane-a".to_string()),
                },
                DiffRow {
                    kind: RowKind::Hunk,
                    old_line: None,
                    new_line: None,
                    text: "@@ -8 +8 @@".to_string(),
                    hunk: Some(1),
                    lane: Some("lane-b".to_string()),
                },
            ]),
        );
        assert_eq!(store.pane, Pane::Changes);
        store.apply_key("j");
        assert_eq!(store.hunk_cursor, 1);
        assert_eq!(store.hunk_row(), Some(2));
        store.apply_key("k");
        assert_eq!(store.hunk_cursor, 0);
        store.apply_key("up");
        assert_eq!(store.hunk_cursor, 0);
    }

    #[test]
    fn command_r_asks_for_a_refresh_and_enter_focuses_a_branch() {
        let (_backend, mut store) = ready();
        assert_eq!(store.apply_key("cmd-r"), Effect::Refresh);
        store.pane = Pane::Branches;
        store.cursors[Pane::Branches.index()] = 0;
        assert_eq!(store.apply_key("enter"), Effect::None);
        assert!(matches!(
            store.side,
            SideFocus::Branch {
                stack: 0,
                branch: 0
            }
        ));
        assert!(store.detail_open());
        assert_eq!(store.pane, Pane::Detail);
        let before = store.lane_rows().len();
        store.toggle_branch_collapsed(0);
        assert!(store.lane_rows().len() < before);
    }

    #[test]
    fn mismatch_sets_the_banner_and_does_not_open() {
        let backend = FakeBackend::mismatched("9.9.9");
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        let banner = store.banner().expect("banner");
        assert!(banner.contains("9.9.9"), "{banner}");
        assert!(banner.contains("0.22.3"), "{banner}");
        assert!(store.workspace.is_none());
        assert!(store.projects.is_empty());
        assert!(store.blocks_open());
        assert!(!store.strip_text().is_empty());
    }

    #[test]
    fn a_dev_channel_build_sets_the_banner_and_does_not_open() {
        struct WrongChannel;

        impl gb_bridge::Settings for WrongChannel {}

        impl Backend for WrongChannel {
            fn environment(&self) -> gb_bridge::Result<Environment> {
                Err(gb_bridge::BridgeError {
                    kind: gb_bridge::BridgeErrorKind::Compat,
                    message: gb_bridge::WRONG_CHANNEL_MESSAGE.to_string(),
                    hint: None,
                })
            }

            fn list_projects(&self) -> gb_bridge::Result<gb_bridge::ProjectList> {
                unreachable!("environment already failed");
            }

            fn open_project(&self, _id: &str) -> gb_bridge::Result<WorkspaceSummary> {
                unreachable!("environment already failed");
            }

            fn watch(
                &self,
                _id: &str,
                _on_change: Box<dyn Fn(BridgeEvent) + Send + Sync>,
            ) -> gb_bridge::Result<gb_bridge::WatchHandle> {
                unreachable!("environment already failed");
            }
        }

        let mut store = Store::default();
        store.apply(load(&WrongChannel, Some("demo-project")));
        assert_eq!(
            store.banner().as_deref(),
            Some(gb_bridge::WRONG_CHANNEL_MESSAGE)
        );
        assert!(store.workspace.is_none());
        assert!(store.blocks_open());
    }

    #[test]
    fn groups_follow_lane_order_and_a_refresh_can_replace_commits() {
        let (backend, mut store) = ready();
        let names: Vec<_> = store
            .change_groups()
            .into_iter()
            .map(|group| group.name)
            .collect();
        assert_eq!(names.first().map(String::as_str), Some("lane-a"));
        assert!(names.iter().any(|name| name == "Split"));
        assert!(names.iter().any(|name| name == "Unassigned"));
        backend.set_workspace(WorkspaceSummary {
            project_id: "demo-project".to_string(),
            target_branch: Some("refs/remotes/origin/main".to_string()),
            stacks: vec![StackSummary {
                branches: vec![BranchSummary {
                    name: "lane-a".to_string(),
                    tip_sha: "abc1234".to_string(),
                    tip_full: one_commit("next").sha.clone(),
                    commits: vec![one_commit("next"), one_commit("older")],
                }],
            }],
        });
        let mut loaded = load(&backend, Some("demo-project"));
        loaded.worktree = store.worktree.as_deref().cloned();
        store.apply(loaded);
        assert_eq!(store.workspace.unwrap().stacks[0].commit_count(), 2);
    }

    #[test]
    fn tree_nests_in_a_stable_order_and_collapse_keeps_counts() {
        let files = vec![
            change("src/b.rs", 1, 0),
            change("README.md", 2, 1),
            change("src/a.rs", 3, 0),
            change("src/sub/c.rs", 4, 2),
            change("zzz/a.rs", 1, 1),
        ];
        let tree = build_tree(&files);
        let names: Vec<_> = tree.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, vec!["src", "zzz", "README.md"]);
        assert_eq!(tree[0].file_count, 3);
        assert_eq!(tree[0].additions, 8);
        assert_eq!(tree[0].deletions, 2);
        let src_names: Vec<_> = tree[0]
            .children
            .iter()
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(src_names, vec!["sub", "a.rs", "b.rs"]);
        let reversed = build_tree(&files.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(
            tree.iter().map(|node| &node.path).collect::<Vec<_>>(),
            reversed.iter().map(|node| &node.path).collect::<Vec<_>>()
        );
        let mut collapsed = BTreeSet::new();
        collapsed.insert("src".to_string());
        let rows = visible_tree(&tree, &collapsed);
        assert!(rows.iter().any(|row| {
            row.path == "src" && !row.expanded && row.file_count == 3 && row.additions == 8
        }));
        assert!(!rows.iter().any(|row| row.path == "src/a.rs"));
        assert!(rows.iter().any(|row| row.path == "zzz/a.rs"));
        let open = visible_tree(&tree, &BTreeSet::new());
        let src_files: Vec<_> = open
            .iter()
            .filter(|row| row.path.starts_with("src"))
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(
            src_files,
            vec!["src", "src/sub", "src/sub/c.rs", "src/a.rs", "src/b.rs"]
        );
    }

    #[test]
    fn the_view_toggle_keeps_the_file_and_collapse_lands_on_the_folder() {
        let (_backend, mut store) = ready();
        store.set_view(ViewMode::Tree);
        assert!(store.focus_file("src/app.rs").is_some());
        let row = store.tree_rows()[store.cursor(Pane::Uncommitted)].clone();
        assert_eq!(row.path, "src/app.rs");
        assert!(row.file.is_some());
        store.toggle_dir("src");
        let row = store.tree_rows()[store.cursor(Pane::Uncommitted)].clone();
        assert_eq!(row.path, "src");
        assert!(!row.expanded);
        assert!(row.file_count >= 2);
        store.apply_key("t");
        assert_eq!(store.view, ViewMode::List);
        store.apply_key("t");
        assert_eq!(store.view, ViewMode::Tree);
        assert_eq!(
            store.tree_rows()[store.cursor(Pane::Uncommitted)].path,
            "src/app.rs"
        );
    }

    #[test]
    fn file_focus_switches_between_the_worktree_and_a_commit() {
        let (_backend, mut store) = ready();
        store.pane = Pane::Uncommitted;
        let effect = store.apply_key("enter");
        let Effect::ShowDiff(DiffRequest::Worktree { path }) = effect else {
            panic!("expected a worktree diff");
        };
        assert!(store.changes_heading().unwrap().starts_with("Uncommitted"));
        assert!(store.changes_heading().unwrap().contains(&path));
        let sha = store.commit_sha_at(0).unwrap();
        assert!(matches!(store.focus_commit(&sha), Effect::ShowCommit(_)));
        assert_eq!(store.focused_kind(), FocusedKind::Commit);
        assert!(store.detail_open());
        store.set_commit(
            &sha,
            Ok(CommitDetails {
                sha: sha.clone(),
                short_sha: sha.chars().take(7).collect(),
                message: "Subject\n\nBody line".to_string(),
                author: "Ada".to_string(),
                committer: "Ada".to_string(),
                authored_at: 10,
                committed_at: 12,
                parents: vec!["parent".to_string()],
                files: vec![CommitFile {
                    path: "src/lane.rs".to_string(),
                    status: FileStatus::Added,
                    additions: 4,
                    deletions: 1,
                }],
            }),
        );
        store.pane = Pane::Detail;
        store.cursors[Pane::Detail.index()] = 0;
        let effect = store.open_current();
        assert!(matches!(
            effect,
            Effect::ShowDiff(DiffRequest::Commit { path, .. }) if path == "src/lane.rs"
        ));
        assert!(matches!(store.shown, ShownFile::Commit { .. }));
        assert!(store.detail_open(), "the commit stays open");
        assert!(store.focus_file("README.md").is_some());
        assert!(matches!(
            store.shown,
            ShownFile::Worktree { ref path } if path == "README.md"
        ));
        store.apply_key("escape");
        assert!(!store.detail_open());
        assert!(matches!(store.shown, ShownFile::Worktree { .. }));
        store.apply_key("escape");
        assert!(matches!(store.shown, ShownFile::None));
        assert_eq!(store.focused_kind(), FocusedKind::None);
    }

    #[test]
    fn branch_focus_fills_the_split_and_escape_removes_it() {
        let (_backend, mut store) = ready();
        assert!(store.focus_branch("lane-b"));
        assert_eq!(store.panes().len(), 4);
        assert_eq!(store.pane, Pane::Detail);
        let info = store.branch_info().expect("branch");
        assert_eq!(info.name, "lane-b");
        assert_eq!(info.stack_label, "Stack 2");
        assert_eq!(info.commit_count, 2);
        assert_eq!(info.push_state.as_deref(), Some("Partly pushed"));
        assert!(!info.authors.is_empty());
        assert!(info.earliest.is_some() && info.latest.is_some());
        assert_eq!(info.commits.len(), 2);
        store.pane = Pane::Branches;
        store.apply_key("tab");
        assert_eq!(store.pane, Pane::Detail);
        store.apply_key("left");
        assert_eq!(store.pane, Pane::Branches);
        store.apply_key("escape");
        assert!(!store.detail_open());
        assert_eq!(store.panes().len(), 3);
        assert!(store.branch_info().is_none());
    }

    #[test]
    fn the_project_menu_moves_and_selects_without_moving_the_file_cursor() {
        let (_backend, mut store) = ready();
        store.pane = Pane::Uncommitted;
        assert_eq!(store.apply_key("cmd-p"), Effect::None);
        assert!(store.project_menu);
        assert_eq!(store.project_cursor(), 0);
        store.apply_key("down");
        assert_eq!(store.cursor(Pane::Uncommitted), 0);
        // The row under the only project is "Add local repository…".
        assert_eq!(store.project_cursor(), 1);
        store.projects.push(ProjectSummary {
            id: "other".to_string(),
            title: "other".to_string(),
            path: "/tmp/other".to_string(),
        });
        store.apply_key("up");
        store.apply_key("down");
        assert_eq!(store.project_cursor(), 1);
        assert_eq!(
            store.apply_key("enter"),
            Effect::SelectProject("other".to_string())
        );
        assert!(!store.project_menu);

        store.apply_key("cmd-p");
        store.apply_key("down");
        store.apply_key("escape");
        assert!(!store.project_menu);
        assert_eq!(store.selected.as_deref(), Some("demo-project"));
        store.set_project_menu(true);
        assert_eq!(store.project_cursor(), 0);
        assert_eq!(
            store.choose_project_at(1),
            Effect::SelectProject("other".to_string())
        );
        assert!(!store.project_menu);

        let backend = FakeBackend::mismatched("9.9.9");
        let mut blocked = Store::default();
        blocked.apply(load(&backend, None));
        blocked.projects.push(ProjectSummary {
            id: "demo-project".to_string(),
            title: "fixture".to_string(),
            path: "/tmp/fixture".to_string(),
        });
        blocked.apply_key("cmd-p");
        assert_eq!(blocked.apply_key("enter"), Effect::None);
        assert!(blocked.project_menu);
        assert!(blocked.blocks_open());
    }

    #[test]
    fn project_hints_cover_setup_and_a_project_that_must_be_opened_first() {
        let (_backend, mut store) = ready();
        std::sync::Arc::make_mut(store.workspace.as_mut().unwrap())
            .target_branch
            .take();
        store.sync_hints();
        assert_eq!(
            store.project_hint("demo-project").as_deref(),
            Some("Needs setup")
        );
        store.set_error(Some(
            "Open this project in GitButler first (pending migration)".to_string(),
        ));
        assert_eq!(
            store.project_hint("demo-project").as_deref(),
            Some("Open in GitButler first")
        );
    }

    fn paths_in_folder_scan(worktree: &WorktreeView, folder: &str) -> Vec<String> {
        let prefix = format!("{folder}/");
        worktree
            .files
            .iter()
            .filter(|file| file.path.starts_with(&prefix))
            .map(|file| file.path.clone())
            .collect()
    }

    fn fully_marked_scan(store: &Store, folder: &str) -> bool {
        let paths = paths_in_folder_scan(store.worktree.as_ref().expect("worktree"), folder);
        !paths.is_empty() && paths.iter().all(|path| store.marked.contains(path))
    }

    #[test]
    fn folder_mark_helpers_match_a_linear_scan_including_nested_folders() {
        let files = vec![
            change("src/b.rs", 1, 0),
            change("src/sub/c.rs", 2, 0),
            change("src/sub/deep/d.rs", 3, 0),
            change("README.md", 1, 1),
            change("zzz/a.rs", 1, 1),
        ];
        let mut store = Store::seeded(
            Some(Arc::new(WorktreeView {
                files,
                changed_paths: Vec::new(),
            })),
            None,
        );
        let folders = [
            "src",
            "src/sub",
            "src/sub/deep",
            "zzz",
            "missing",
            "src/sub/",
        ];
        for folder in folders {
            assert_eq!(
                store.folder_paths_vec(folder),
                paths_in_folder_scan(store.worktree.as_ref().unwrap(), folder),
                "{folder}"
            );
            assert_eq!(
                store.folder_fully_marked(folder),
                fully_marked_scan(&store, folder),
                "{folder}"
            );
        }
        // Nothing marked, then everything under src, then a partial mark.
        for path in ["src/b.rs", "src/sub/c.rs", "src/sub/deep/d.rs"] {
            store.mark_click(path, false);
        }
        assert!(store.folder_fully_marked("src"));
        assert!(store.folder_fully_marked("src/sub"));
        assert!(store.folder_fully_marked("src/sub/deep"));
        assert!(!store.folder_fully_marked("zzz"));
        store.mark_click("src/sub/c.rs", false);
        assert!(!store.folder_fully_marked("src/sub"));
        assert!(store.folder_fully_marked("src/sub/deep"));
        // A replaced worktree refreshes the index (no stale folders).
        let replacement = vec![change("other/e.rs", 1, 0)];
        store.worktree = Some(Arc::new(WorktreeView {
            files: replacement,
            changed_paths: Vec::new(),
        }));
        assert_eq!(
            store.folder_paths_vec("other"),
            paths_in_folder_scan(store.worktree.as_ref().unwrap(), "other")
        );
        assert!(store.folder_paths_vec("src").is_empty());
        assert!(!store.folder_fully_marked("src"));
    }

    #[test]
    fn path_lookups_and_hunk_headers_match_a_linear_find() {
        let (_backend, store) = ready();
        let worktree = store.worktree.as_ref().unwrap();
        for (index, file) in worktree.files.iter().enumerate() {
            assert_eq!(store.file_index_by_path(&file.path), Some(index));
            let got = store.file_hunk_headers(&file.path);
            let want: Vec<String> = file.hunks.iter().map(|hunk| hunk.header.clone()).collect();
            assert_eq!(*got, want, "{}", file.path);
        }
        assert_eq!(store.file_index_by_path("missing.rs"), None);
        assert!(store.file_hunk_headers("missing.rs").is_empty());
    }

    fn branch_info_scan(store: &Store) -> Option<BranchFocusInfo> {
        let SideFocus::Branch { stack, branch } = &store.side else {
            return None;
        };
        let summary = &store
            .workspace
            .as_ref()?
            .stacks
            .get(*stack)?
            .branches
            .get(*branch)?;
        let mut authors: Vec<String> = Vec::new();
        let mut earliest = None;
        let mut latest = None;
        for commit in &summary.commits {
            if !authors.iter().any(|author| author == &commit.author) {
                authors.push(commit.author.clone());
            }
            earliest =
                Some(earliest.map_or(commit.authored_at, |at: i64| at.min(commit.authored_at)));
            latest = Some(latest.map_or(commit.authored_at, |at: i64| at.max(commit.authored_at)));
        }
        Some(BranchFocusInfo {
            name: summary.name.clone(),
            tip_sha: summary.tip_sha.clone(),
            tip_full: summary.tip_full.clone(),
            stack_label: format!("Stack {}", stack + 1),
            commit_count: summary.commits.len(),
            push_state: push_state(&summary.commits),
            authors,
            earliest,
            latest,
            commits: Arc::new(
                summary
                    .commits
                    .iter()
                    .map(|commit| CommitLine {
                        sha: commit.sha.clone(),
                        short_sha: commit.short_sha.clone(),
                        title: commit.title.clone(),
                    })
                    .collect(),
            ),
        })
    }

    #[test]
    fn branch_info_matches_the_scan_dedups_authors_and_refreshes() {
        let (_backend, mut store) = ready();
        for name in ["lane-a", "lane-b", "lane-c"] {
            assert!(store.focus_branch(name));
            assert_eq!(store.branch_info(), branch_info_scan(&store), "{name}");
        }

        // Authors keep first-seen order and drop repeats.
        let commit = |sha: &str, author: &str| CommitSummary {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            title: format!("title {sha}"),
            author: author.to_string(),
            authored_at: 10,
            conflicted: false,
            pushed: false,
        };
        let workspace = WorkspaceSummary {
            project_id: "t".to_string(),
            target_branch: None,
            stacks: vec![StackSummary {
                branches: vec![BranchSummary {
                    name: "multi".to_string(),
                    tip_sha: "c4".to_string(),
                    tip_full: "c4full".to_string(),
                    commits: vec![
                        commit("c1", "Bea"),
                        commit("c2", "Ada"),
                        commit("c3", "Bea"),
                        commit("c4", "Cid"),
                    ],
                }],
            }],
        };
        store.workspace = Some(Arc::new(workspace));
        assert!(store.focus_branch("multi"));
        let info = store.branch_info().expect("info");
        assert_eq!(info.authors, vec!["Bea", "Ada", "Cid"]);
        assert_eq!(info, branch_info_scan(&store).unwrap());

        // A replaced workspace refreshes the cached facts (stale generation).
        let workspace = WorkspaceSummary {
            project_id: "t".to_string(),
            target_branch: None,
            stacks: vec![StackSummary {
                branches: vec![BranchSummary {
                    name: "multi".to_string(),
                    tip_sha: "d1".to_string(),
                    tip_full: "d1full".to_string(),
                    commits: vec![commit("d1", "Eve")],
                }],
            }],
        };
        store.workspace = Some(Arc::new(workspace));
        let info = store.branch_info().expect("info");
        assert_eq!(info.commit_count, 1);
        assert_eq!(info.authors, vec!["Eve"]);
        assert_eq!(info, branch_info_scan(&store).unwrap());
    }

    #[test]
    fn branch_name_for_matches_the_scan_and_survives_a_refresh() {
        let (_backend, mut store) = ready();
        let sha = store.commit_sha_at(0).expect("commit");
        assert_eq!(store.branch_name_for(&sha), store.branch_name_scan(&sha));
        assert_eq!(
            store.branch_name_for("does-not-exist"),
            store.branch_name_scan("does-not-exist")
        );
        store.workspace = Some(Arc::new(WorkspaceSummary {
            project_id: "demo-project".to_string(),
            target_branch: None,
            stacks: vec![StackSummary {
                branches: vec![BranchSummary {
                    name: "renamed".to_string(),
                    tip_sha: sha.clone(),
                    tip_full: sha.clone(),
                    commits: Vec::new(),
                }],
            }],
        }));
        assert_eq!(store.branch_name_for(&sha), Some("renamed".to_string()));
        assert_eq!(store.branch_name_for(&sha), store.branch_name_scan(&sha));
    }

    #[test]
    fn branch_indexes_and_visible_rows_match_visible_indices_for_every_tab_and_query() {
        let mut store = Store::seeded(None, None);
        store.pages.branch_rows = super::perf::seed_branches(30);
        for tab in [
            pages::BranchTab::Local,
            pages::BranchTab::Remote,
            pages::BranchTab::PullRequests,
        ] {
            for query in ["", "topic-0001", "TOPIC-0002", "perf", "missing", " perf "] {
                store.set_branch_filter(query.to_string());
                store.set_branch_tab(tab);
                let want = pages::visible_indices(&store.pages.branch_rows, tab, query);
                assert_eq!(*store.branch_indexes(), want, "{tab:?} {query:?}");
                let want_rows: Vec<BranchRow> = want
                    .iter()
                    .filter_map(|index| store.pages.branch_rows.get(*index).cloned())
                    .collect();
                assert_eq!(*store.visible_branches(), want_rows, "{tab:?} {query:?}");
            }
        }
    }

    #[test]
    fn typing_defers_the_branch_refilter_until_the_commit() {
        let mut store = Store::seeded(None, None);
        store.pages.branch_rows = super::perf::seed_branches(30);
        store.pages.tab = pages::BranchTab::Local;
        let before = store.branch_indexes();
        assert_eq!(store.branch_filter_rebuilds(), 1);
        assert!(!store.branch_filter_pending());

        // Typing only moves the text box; the lists keep the committed filter.
        store.pages.filter = "topic-0001".to_string();
        assert!(store.branch_filter_pending());
        assert_eq!(*store.branch_indexes(), *before);
        assert_eq!(store.branch_filter_rebuilds(), 1);

        store.commit_branch_filter();
        assert!(!store.branch_filter_pending());
        assert_eq!(
            *store.branch_indexes(),
            pages::visible_indices(
                &store.pages.branch_rows,
                pages::BranchTab::Local,
                "topic-0001"
            )
        );
        assert_eq!(store.branch_filter_rebuilds(), 2);

        // Dev commands commit at once instead of waiting for the debounce.
        store.set_branch_filter("topic-0002".to_string());
        assert!(!store.branch_filter_pending());
        assert_eq!(
            *store.branch_indexes(),
            pages::visible_indices(
                &store.pages.branch_rows,
                pages::BranchTab::Local,
                "topic-0002"
            )
        );
        assert_eq!(store.branch_filter_rebuilds(), 3);

        // A page reset syncs the committed filter too.
        store.pages.filter = "stale".to_string();
        store.on_pages_project_change();
        assert!(!store.branch_filter_pending());
        assert_eq!(store.branch_filter_rebuilds(), 3);
    }

    fn hunk(header: &str, lane: Option<&str>) -> AssignedHunk {
        AssignedHunk {
            header: header.to_string(),
            lane: lane.map(str::to_string),
        }
    }

    #[test]
    fn cached_assignment_labels_match_the_bridge_method() {
        let shapes: Vec<Vec<AssignedHunk>> = vec![
            Vec::new(),
            vec![hunk("@@ -1,1 +1,2 @@", None)],
            vec![hunk("@@ -1,1 +1,2 @@", Some("lane-a"))],
            vec![
                hunk("@@ -1,1 +1,2 @@", Some("lane-a")),
                hunk("@@ -9,1 +9,2 @@", Some("lane-a")),
            ],
            vec![
                hunk("@@ -1,1 +1,2 @@", Some("lane-a")),
                hunk("@@ -9,1 +9,2 @@", None),
                hunk("@@ -20,1 +20,2 @@", Some("lane-a")),
            ],
            vec![
                hunk("@@ -1,1 +1,2 @@", Some("lane-a")),
                hunk("@@ -9,1 +9,2 @@", Some("lane-b")),
            ],
            vec![
                hunk("@@ -1,1 +1,2 @@", Some("lane-a")),
                hunk("@@ -9,1 +9,2 @@", Some("lane-b")),
                hunk("@@ -20,1 +20,2 @@", Some("lane-a")),
            ],
        ];
        for hunks in shapes {
            let file = FileChange {
                path: "src/a.rs".to_string(),
                old_path: None,
                status: FileStatus::Modified,
                additions: 2,
                deletions: 1,
                hunks: hunks.clone(),
            };
            assert_eq!(assignment_label_for(&file), file.assignment_label());
        }
    }

    fn listed_row(name: &str, place: BranchPlace) -> BranchRow {
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
            applied: false,
            stack: "unapplied".to_string(),
            stack_branches: vec![name.to_string()],
            tip_sha: format!("{name}-tip"),
            tip_short: name.chars().take(7).collect(),
            last_commit: "Wire the lane header".to_string(),
            author: "Ada <ada@example.com>".to_string(),
            updated_at_ms: None,
            ahead: Some(1),
            behind: Some(0),
        }
    }

    #[test]
    fn replacing_branch_rows_moves_the_branch_stamp() {
        let (_backend, mut store) = ready();
        let rows = vec![listed_row("topic-a", BranchPlace::Local)];
        store.set_branch_rows(rows.clone());
        store.set_branch_filter("topic".to_string());
        let warm = store.branch_indexes();
        let stamp = store.branch_stamp();
        // Identical rows again: length and pointer stay equal, so only the
        // generation counter can move the stamp and invalidate the caches.
        store.set_branch_rows(rows);
        assert_ne!(store.branch_stamp(), stamp);
        assert_eq!(store.branch_indexes(), warm);
    }

    fn hunked_worktree(path: &str, lane: Option<&str>) -> WorktreeView {
        WorktreeView {
            files: vec![FileChange {
                path: path.to_string(),
                old_path: None,
                status: FileStatus::Modified,
                additions: 3,
                deletions: 1,
                hunks: vec![hunk("@@ -1,1 +1,2 @@", lane)],
            }],
            changed_paths: Vec::new(),
        }
    }

    #[test]
    fn replacing_the_worktree_without_other_apis_serves_new_content() {
        let (_backend, mut store) = ready();
        store.set_worktree(Some(Arc::new(hunked_worktree(
            "src/old.rs",
            Some("lane-a"),
        ))));
        let _ = store.change_groups();
        let _ = store.tree_rows();
        let _ = store.lane_labels();
        let _ = store.file_hunk_headers("src/old.rs");
        assert_eq!(store.file_index_by_path("src/old.rs"), Some(0));

        // Drop the old Arc before allocating the replacement so the allocator
        // may hand back the very same address; the generation counter must
        // still invalidate every worktree-keyed cache.
        let old = store.worktree.take();
        drop(old);
        store.set_worktree(Some(Arc::new(hunked_worktree(
            "src/new.rs",
            Some("lane-b"),
        ))));

        assert_eq!(store.file_index_by_path("src/new.rs"), Some(0));
        assert_eq!(store.file_index_by_path("src/old.rs"), None);
        let groups = store.change_groups();
        assert!(
            groups.iter().all(|group| group.name != "lane-a"),
            "{groups:?}"
        );
        assert_eq!(
            store.lane_labels().first().map(String::as_str),
            Some("lane-b")
        );
        assert_eq!(store.file_hunk_headers("src/new.rs").len(), 1);
        let tree = store.tree_rows();
        assert!(tree.iter().any(|row| row.path == "src/new.rs"), "{tree:?}");
    }
}
