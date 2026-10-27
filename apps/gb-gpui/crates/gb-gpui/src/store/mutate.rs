//! Mutation planning with no GPUI types.
//!
//! The window executes an [`Action`] through [`perform_action`], then
//! [`Store::accept`] keeps the cursor, the undo label, and the composer.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use gb_bridge::{
    Anchor, Backend, BranchPlace, FORGE_SIGN_IN, FileChange, ForcePush, HookReport, HookStatus,
    HunkRef, SelectionSpec, WorkspaceSummary, WorktreeView,
};

use super::{Effect, Pane, RowKind, ShownFile, SideFocus, SpecsKey, Store};

/// One change the window should run on the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Assign {
        selections: Vec<SelectionSpec>,
        lane: Option<String>,
    },
    Discard {
        selections: Vec<SelectionSpec>,
    },
    /// Create the lane, then optionally assign and commit onto it.
    CreateLane {
        name: String,
        assign: Vec<SelectionSpec>,
        commit: Option<CommitDraft>,
    },
    Commit {
        lane: String,
        selections: Vec<SelectionSpec>,
        message: String,
    },
    Amend {
        sha: String,
        selections: Vec<SelectionSpec>,
    },
    Reword {
        sha: String,
        message: String,
    },
    Squash {
        subject: String,
        target: String,
    },
    Uncommit {
        sha: String,
    },
    MoveCommit {
        sha: String,
        anchor: Anchor,
    },
    InsertBlank {
        anchor: Anchor,
    },
    Undo,
}

/// Message and files for a commit that follows a new lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDraft {
    pub selections: Vec<SelectionSpec>,
    pub message: String,
}

/// What the bridge returned, plus how the window should move focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationDone {
    pub workspace: WorkspaceSummary,
    pub worktree: WorktreeView,
    /// Pushed in order. The last one is what Undo reverts.
    pub labels: Vec<String>,
    pub pop_undo: bool,
    pub hook: Option<String>,
    pub notice: Option<String>,
    pub dropped_paths: Vec<String>,
    pub focus_sha: Option<String>,
    pub reload_diff: bool,
    pub created_lane: Option<String>,
    pub clear_draft: bool,
}

/// Which field receives typed keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Body,
    LaneName,
}

/// Compact commit box. A fixed height in the view, so typing does not jump.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    pub open: bool,
    pub lane: Option<String>,
    pub new_lane: bool,
    pub lane_name: String,
    pub title: String,
    pub body: String,
    pub field: Option<Field>,
    pub hook: Option<String>,
    /// Inline reason when a suggestion could not be written.
    pub suggest_error: Option<String>,
}

/// Why a row shows a spinner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyOp {
    pub op: String,
    pub row: Option<String>,
}

/// A popover the keyboard can walk. Enter chooses the cursor row.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionMenu {
    pub kind: MenuKind,
    pub cursor: usize,
    pub naming: bool,
    pub draft: String,
    /// Window point where the menu should open. The view clamps it.
    pub anchor_x: f32,
    pub anchor_y: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuKind {
    MoveFiles {
        paths: Vec<String>,
    },
    MoveHunk {
        path: String,
        header: String,
    },
    MoveCommit {
        sha: String,
    },
    CommitActions {
        sha: String,
    },
    ComposerLane,
    AmendInto,
    /// Move and discard for one file, a marked set, or a group.
    FileActions {
        paths: Vec<String>,
    },
    HunkActions {
        path: String,
        header: String,
    },
    LaneActions {
        name: String,
    },
    ListedBranch {
        name: String,
    },
}

/// Destructive work waits here until Enter or the confirm button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConfirm {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub action: Action,
}

/// One row in an open menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub label: String,
    pub tip: String,
    pub enabled: bool,
    /// Drawn in the danger color. Choosing it still confirms when the action asks.
    pub destructive: bool,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    AssignLane(Option<String>),
    NewLane,
    MoveToLane(String),
    MoveEarlier,
    MoveLater,
    Reword,
    Squash,
    Uncommit,
    OpenMove,
    InsertBlank,
    Amend,
    EditCommit,
    PickLane(String),
    PickNewLane,
    AmendCommit(String),
    OpenMoveFiles,
    DiscardFiles,
    OpenMoveHunk,
    DiscardHunk,
    OpenPush(String),
    ForceLease(String),
    ForcePush(String),
    Publish(String),
    UnapplyLane(String),
    InsertBlankOn(String),
    ApplyBranch(String),
    UnapplyBranch(String),
    DeleteBranch(String),
    ResolveConflicts(String),
}

/// Inline reword. Enter saves, Escape cancels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewordDraft {
    pub sha: String,
    pub draft: String,
}

impl Store {
    pub fn reset_mutation_ui(&mut self) {
        self.bump_marks();
        self.marked.clear();
        self.mark_anchor = None;
        self.marked_hunks.clear();
        self.menu = None;
        self.confirm = None;
        self.composer = Composer::default();
        self.reword = None;
        self.busy = None;
        self.suggest_busy = false;
        self.message_hold = None;
        self.mutation_error = None;
        self.toast = None;
        self.undo_labels.clear();
    }

    pub fn wants_text(&self) -> bool {
        self.composer.field.is_some()
            || self.reword.is_some()
            || self.menu.as_ref().is_some_and(|menu| menu.naming)
    }

    pub fn undo_caption(&self) -> Option<String> {
        self.undo_labels
            .last()
            .map(|label| format!("Undo: {label}"))
    }

    #[cfg(test)]
    pub fn row_busy(&self, key: &str) -> bool {
        self.busy
            .as_ref()
            .is_some_and(|busy| busy.row.as_deref() == Some(key))
    }

    pub fn file_marked(&self, path: &str) -> bool {
        self.marked.contains(path)
    }

    pub fn hunk_marked(&self, path: &str, header: &str) -> bool {
        self.hunk_mark_index()
            .get(path)
            .is_some_and(|headers| headers.contains(header))
    }

    /// Keys that belong to a mutation overlay. `None` lets the list move.
    pub fn apply_mutation_key(&mut self, key: &str) -> Option<Effect> {
        if self.mutation_blocked().is_some()
            && matches!(
                key,
                "cmd-z"
                    | "cmd-Z"
                    | "m"
                    | "d"
                    | "backspace"
                    | "c"
                    | "r"
                    | "s"
                    | "u"
                    | "i"
                    | "a"
                    | "x"
            )
        {
            self.note_edit_lock();
            return Some(Effect::None);
        }
        if key == "cmd-z" || key == "cmd-Z" {
            if self.confirm.is_some() {
                return Some(Effect::None);
            }
            return Some(self.request_undo());
        }
        if self.project_menu {
            return None;
        }
        if self.confirm.is_some() {
            return Some(self.apply_confirm_key(key));
        }
        if self.menu.as_ref().is_some_and(|menu| menu.naming) || self.wants_text() {
            return Some(self.apply_text_key(key));
        }
        if self.menu.is_some() {
            return Some(self.apply_menu_key(key));
        }
        if self.reword.is_some() {
            return Some(self.apply_text_key(key));
        }
        match key {
            "escape" if self.toast.is_some() || !self.activity.visible_toasts().is_empty() => {
                self.toast = None;
                self.activity.dismiss_toasts();
                Some(Effect::None)
            }
            "enter" if self.message_hold.is_some() && self.composer.field.is_none() => {
                self.replace_held_message();
                Some(Effect::None)
            }
            "escape" if self.message_hold.is_some() && self.composer.field.is_none() => {
                self.keep_typed_message();
                Some(Effect::None)
            }
            "escape" if self.composer.field.is_some() => {
                self.composer.field = None;
                Some(Effect::None)
            }
            "escape" if self.composer.open => {
                self.composer.open = false;
                self.composer.field = None;
                Some(Effect::None)
            }
            "m" => Some(self.open_move_for_focus()),
            "space" => {
                self.toggle_focused_mark();
                Some(Effect::None)
            }
            "d" | "backspace" => Some(self.request_discard()),
            "c" => {
                self.toggle_composer();
                Some(Effect::None)
            }
            "r" => Some(self.start_reword()),
            "s" => Some(self.request_squash()),
            "u" => Some(self.request_uncommit()),
            "i" => Some(self.request_blank()),
            "a" => Some(self.request_amend_focused()),
            "x" => Some(self.open_commit_actions()),
            _ => None,
        }
    }

    pub fn toggle_composer(&mut self) {
        self.composer.open = !self.composer.open;
        if self.composer.open {
            self.ensure_composer_lane();
            self.composer.field = Some(Field::Title);
        } else {
            self.composer.field = None;
            if matches!(
                self.menu.as_ref().map(|menu| &menu.kind),
                Some(MenuKind::ComposerLane) | Some(MenuKind::AmendInto)
            ) {
                self.menu = None;
            }
        }
    }

    pub fn set_composer_open(&mut self, open: bool) {
        if self.composer.open != open {
            self.toggle_composer();
        }
        if open {
            self.ensure_composer_lane();
        }
    }

    pub fn focus_field(&mut self, field: Field) {
        self.composer.open = true;
        self.ensure_composer_lane();
        self.composer.field = Some(field);
        self.menu = None;
        self.reword = None;
    }

    pub fn lane_names(&self) -> Vec<String> {
        lane_names(self.workspace.as_deref())
    }

    pub fn composer_lane_label(&self) -> String {
        if self.composer.new_lane {
            if self.composer.lane_name.trim().is_empty() {
                "New lane".to_string()
            } else {
                format!("New lane: {}", self.composer.lane_name.trim())
            }
        } else {
            self.resolved_lane()
                .unwrap_or_else(|| "Choose a lane".to_string())
        }
    }

    pub fn ruler_hint(&self) -> String {
        let title = self.composer.title.chars().count();
        let body = self
            .composer
            .body
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        format!("Title {title}/50, body {body}/72")
    }

    pub fn title_over(&self) -> bool {
        self.composer.title.chars().count() > 50
    }

    pub fn body_over(&self) -> bool {
        self.composer
            .body
            .lines()
            .any(|line| line.chars().count() > 72)
    }

    pub fn commit_message(&self) -> String {
        let title = self.composer.title.trim();
        let body = self.composer.body.trim();
        if body.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n\n{body}")
        }
    }

    /// Why Commit stays quiet. `None` means the button can run.
    pub fn commit_block(&self) -> Option<String> {
        if let Some(reason) = self.mutation_blocked() {
            return Some(reason.to_string());
        }
        if self.busy.is_some() {
            return Some("Wait for the current change to finish.".to_string());
        }
        if self.selected.is_none() {
            return Some("Choose a project first.".to_string());
        }
        if self.composer.title.trim().is_empty() {
            return Some("Type a commit title.".to_string());
        }
        if self.composer.new_lane {
            if !valid_lane_name(&self.composer.lane_name) {
                return Some("Type a lane name with no spaces.".to_string());
            }
        } else if self.resolved_lane().is_none() {
            return Some("Choose a lane.".to_string());
        }
        if self.commit_specs().is_empty() {
            return Some("Nothing is selected for this commit.".to_string());
        }
        None
    }

    pub fn commit_summary(&self) -> String {
        let specs = self.commit_specs();
        if specs.is_empty() {
            return "No files selected".to_string();
        }
        let hunks: usize = specs
            .iter()
            .map(|spec| spec.hunks.as_ref().map(Vec::len).unwrap_or(0))
            .sum();
        if specs.len() == 1 {
            let path = &specs[0].path;
            if hunks > 0 {
                return format!(
                    "{hunks} hunk{} in {path}",
                    if hunks == 1 { "" } else { "s" }
                );
            }
            return format!("1 file: {path}");
        }
        if hunks > 0 {
            return format!(
                "{} files, {hunks} hunk{}",
                specs.len(),
                if hunks == 1 { "" } else { "s" }
            );
        }
        format!("{} files", specs.len())
    }

    pub fn plan_commit(&mut self) -> Effect {
        let Some(message) = self.commit_ready_message() else {
            return Effect::None;
        };
        let selections = (*self.commit_specs()).clone();
        if self.composer.new_lane {
            let name = self.composer.lane_name.trim().to_string();
            return Effect::Mutate(Action::CreateLane {
                name,
                assign: Vec::new(),
                commit: Some(CommitDraft {
                    selections,
                    message,
                }),
            });
        }
        let Some(lane) = self.resolved_lane() else {
            return Effect::None;
        };
        Effect::Mutate(Action::Commit {
            lane,
            selections,
            message,
        })
    }

    /// Files and hunks the next commit or amend will take.
    ///
    /// A mark wins. With no marks, every hunk assigned to the composer lane is included.
    /// Memoized per (mark generation, data generation, composer lane).
    pub fn commit_specs(&self) -> Arc<Vec<SelectionSpec>> {
        let Some(worktree) = &self.worktree else {
            return Arc::new(Vec::new());
        };
        let (new_lane, lane) = (self.composer.new_lane, self.resolved_lane());
        let (mark_gen, data_gen) = {
            let derived = self.derived.borrow();
            (derived.mark_gen, derived.data_gen)
        };
        let key = SpecsKey {
            mark_gen,
            data_gen,
            worktree: Arc::as_ptr(worktree) as usize,
            workspace: self
                .workspace
                .as_ref()
                .map(|workspace| Arc::as_ptr(workspace) as usize)
                .unwrap_or(0),
            new_lane,
            lane: lane.clone(),
        };
        if self.derived.borrow().specs_key.as_ref() == Some(&key) {
            return Arc::clone(&self.derived.borrow().specs);
        }
        let specs = if !self.marked.is_empty() || !self.marked_hunks.is_empty() {
            marked_specs(worktree, &self.marked, &self.marked_hunks)
        } else if let Some(lane) = &lane {
            lane_specs(worktree, lane)
        } else {
            Vec::new()
        };
        let specs = Arc::new(specs);
        let mut derived = self.derived.borrow_mut();
        derived.specs_key = Some(key);
        derived.specs = Arc::clone(&specs);
        specs
    }

    pub fn mark_click(&mut self, path: &str, shift: bool) {
        self.bump_marks();
        if shift {
            let anchor = self.mark_anchor.clone().unwrap_or_else(|| path.to_string());
            for path in self.paths_between(&anchor, path) {
                self.marked.insert(path);
            }
            return;
        }
        if !self.marked.remove(path) {
            self.marked.insert(path.to_string());
        }
        self.mark_anchor = Some(path.to_string());
    }

    pub fn toggle_hunk_mark(&mut self, path: &str, header: &str) {
        self.bump_marks();
        let key = (path.to_string(), header.to_string());
        if !self.marked_hunks.remove(&key) {
            self.marked_hunks.insert(key);
        }
    }

    pub fn open_file_menu(&mut self, path: &str) -> Effect {
        let paths = if self.marked.is_empty() {
            vec![path.to_string()]
        } else if self.marked.contains(path) {
            self.marked.iter().cloned().collect()
        } else {
            vec![path.to_string()]
        };
        self.open_menu(MenuKind::MoveFiles { paths });
        Effect::None
    }

    pub fn open_hunk_menu(&mut self, path: &str, header: &str) -> Effect {
        self.open_menu(MenuKind::MoveHunk {
            path: path.to_string(),
            header: header.to_string(),
        });
        Effect::None
    }

    pub fn open_lane_menu(&mut self) {
        self.composer.open = true;
        self.ensure_composer_lane();
        self.open_menu(MenuKind::ComposerLane);
    }

    pub fn open_amend_menu(&mut self) {
        self.open_menu(MenuKind::AmendInto);
    }

    pub fn menu_entries(&self) -> Vec<MenuEntry> {
        let Some(menu) = &self.menu else {
            return Vec::new();
        };
        match &menu.kind {
            MenuKind::MoveFiles { .. } | MenuKind::MoveHunk { .. } => self.assign_entries(),
            MenuKind::MoveCommit { sha } => self.move_commit_entries(sha),
            MenuKind::CommitActions { sha } => self.commit_action_entries(sha),
            MenuKind::ComposerLane => self.composer_lane_entries(),
            MenuKind::AmendInto => self.amend_entries(),
            MenuKind::FileActions { paths } => self.file_action_entries(paths),
            MenuKind::HunkActions { path, header } => self.hunk_action_entries(path, header),
            MenuKind::LaneActions { name } => self.lane_action_entries(name),
            MenuKind::ListedBranch { name } => self.listed_branch_entries(name),
        }
    }

    pub fn menu_title(&self) -> String {
        match self.menu.as_ref().map(|menu| &menu.kind) {
            Some(MenuKind::MoveFiles { paths }) if paths.len() == 1 => {
                format!("Move {}", paths[0])
            }
            Some(MenuKind::MoveFiles { paths }) => format!("Move {} files", paths.len()),
            Some(MenuKind::MoveHunk { path, .. }) => format!("Move hunk in {path}"),
            Some(MenuKind::MoveCommit { sha }) => format!("Move {}", short7(sha)),
            Some(MenuKind::CommitActions { sha }) => format!("Commit {}", short7(sha)),
            Some(MenuKind::ComposerLane) => "Commit to".to_string(),
            Some(MenuKind::AmendInto) => "Amend into".to_string(),
            Some(MenuKind::FileActions { paths }) if paths.len() == 1 => paths[0].clone(),
            Some(MenuKind::FileActions { paths }) => format!("{} files", paths.len()),
            Some(MenuKind::HunkActions { path, .. }) => format!("Hunk in {path}"),
            Some(MenuKind::LaneActions { name } | MenuKind::ListedBranch { name }) => name.clone(),
            None => String::new(),
        }
    }

    pub fn choose_menu(&mut self) -> Effect {
        if self.menu.as_ref().is_some_and(|menu| menu.naming) {
            return self.finish_new_lane();
        }
        let entries = self.menu_entries();
        let Some(menu) = &self.menu else {
            return Effect::None;
        };
        let Some(entry) = entries.get(menu.cursor) else {
            return Effect::None;
        };
        if !entry.enabled {
            self.mutation_error = Some(entry.tip.clone());
            self.toast_bad = true;
            self.toast = Some(entry.tip.clone());
            return Effect::None;
        }
        let kind = entry.kind.clone();
        self.activate_entry(kind)
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    pub fn confirm_yes(&mut self) -> Effect {
        let Some(confirm) = self.confirm.take() else {
            return Effect::None;
        };
        Effect::Mutate(confirm.action)
    }

    pub fn confirm_no(&mut self) {
        self.confirm = None;
    }

    pub fn request_undo(&mut self) -> Effect {
        if self.busy.is_some() {
            self.toast_bad = true;
            self.toast = Some("Wait for the current change to finish.".to_string());
            return Effect::None;
        }
        if self.undo_labels.is_empty() {
            self.toast_bad = true;
            self.toast = Some("Nothing to undo.".to_string());
            return Effect::None;
        }
        Effect::Mutate(Action::Undo)
    }

    pub fn arm_busy(&mut self, action: &Action) -> bool {
        if self.mutation_blocked().is_some() {
            self.note_edit_lock();
            return false;
        }
        if self.busy.is_some() {
            self.toast_bad = true;
            self.toast = Some("Wait for the current change to finish.".to_string());
            return false;
        }
        self.busy = Some(BusyOp {
            op: action_op(action).to_string(),
            row: action_row(action),
        });
        self.mutation_error = None;
        self.toast = None;
        true
    }

    pub fn fail_busy(&mut self, message: &str) {
        if let Some(id) = self.activity.loading_mutation() {
            self.activity.fail(&id, message);
        }
        self.busy = None;
        self.mutation_error = Some(message.to_string());
        self.toast_bad = true;
        self.toast = Some(message.to_string());
        if self.composer.open {
            self.composer.hook = Some(message.to_string());
        }
    }

    pub fn accept(&mut self, done: MutationDone) -> Effect {
        if let Some(id) = self.activity.loading_mutation() {
            if let Some(notice) = done.notice.clone() {
                self.activity.fail(&id, notice);
            } else {
                let message = self.activity.done_text(&id).to_string();
                self.activity.succeed(&id, message);
            }
        }
        self.workspace = Some(Arc::new(done.workspace));
        self.worktree = Some(Arc::new(done.worktree));
        self.bump_data();
        self.busy = None;
        if done.pop_undo {
            self.undo_labels.pop();
        }
        self.undo_labels.extend(done.labels);
        if let Some(hook) = done.hook {
            self.composer.hook = Some(hook);
        }
        self.mutation_error = done.notice.clone();
        self.toast_bad = true;
        self.toast = done.notice;
        if !done.dropped_paths.is_empty() {
            self.bump_marks();
            for path in &done.dropped_paths {
                self.marked.remove(path);
                self.marked_hunks.retain(|(have, _)| have != path);
            }
        }
        if done.clear_draft {
            self.composer.title.clear();
            self.composer.body.clear();
            self.composer.field = None;
        }
        if let Some(name) = done.created_lane {
            self.composer.new_lane = false;
            self.composer.lane = Some(name);
            self.composer.lane_name.clear();
        }
        self.menu = None;
        self.confirm = None;
        self.reword = None;
        self.clamp();
        if let Some(sha) = done.focus_sha.clone()
            && self.commit_exists(&sha)
        {
            return self.focus_commit(&sha);
        }
        if done.reload_diff {
            return self.reload_shown();
        }
        Effect::None
    }

    /// Fill the composer, then plan the commit. Used by the dev socket.
    #[cfg(any(test, feature = "devtools"))]
    pub fn commit_with(
        &mut self,
        title: &str,
        body: &str,
        lane: Option<&str>,
    ) -> Result<Effect, String> {
        self.composer.open = true;
        self.composer.title = title.to_string();
        self.composer.body = body.to_string();
        if let Some(lane) = lane {
            if lane == "new" {
                self.composer.new_lane = true;
            } else {
                self.composer.new_lane = false;
                self.composer.lane = Some(lane.to_string());
            }
        }
        self.ensure_composer_lane();
        match self.commit_block() {
            Some(reason) => Err(reason),
            None => Ok(self.plan_commit()),
        }
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn assign_paths(&mut self, paths: &[String], lane: Option<&str>) -> Result<Effect, String> {
        let selections = self.specs_for(paths);
        if selections.is_empty() {
            return Err(format!("No uncommitted change for {}", paths.join(", ")));
        }
        let lane = normalize_lane(lane);
        Ok(Effect::Mutate(Action::Assign { selections, lane }))
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn reword_to(&mut self, sha: &str, message: &str) -> Result<Effect, String> {
        let sha = self.resolve_sha(sha);
        if !self.commit_exists(&sha) {
            return Err(format!("No commit {sha}"));
        }
        if message.trim().is_empty() {
            return Err("Write a commit message first.".to_string());
        }
        Ok(Effect::Mutate(Action::Reword {
            sha,
            message: message.to_string(),
        }))
    }

    #[cfg(feature = "devtools")]
    pub fn squash_pair(&mut self, subject: &str, target: &str) -> Result<Effect, String> {
        let subject = self.resolve_sha(subject);
        let target = self.resolve_sha(target);
        if subject == target {
            return Err("Choose a different commit to squash into.".to_string());
        }
        Ok(Effect::Mutate(Action::Squash { subject, target }))
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn uncommit_sha(&mut self, sha: &str) -> Result<Effect, String> {
        let sha = self.resolve_sha(sha);
        if !self.commit_exists(&sha) {
            return Err(format!("No commit {sha}"));
        }
        Ok(Effect::Mutate(Action::Uncommit { sha }))
    }

    /// Ask before dropping one worktree hunk.
    pub fn ask_discard_hunk(&mut self, path: &str, header: &str) -> Effect {
        let Some(file) = self
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.files.iter().find(|file| file.path == path))
        else {
            return Effect::None;
        };
        let Some(spec) = hunk_spec(file, &[header]) else {
            return Effect::None;
        };
        self.confirm = Some(PendingConfirm {
            title: "Discard this hunk?".to_string(),
            message: format!(
                "The hunk in {path} is dropped from the worktree. Undo can bring it back."
            ),
            confirm_label: "Discard hunk".to_string(),
            action: Action::Discard {
                selections: vec![spec],
            },
        });
        Effect::None
    }

    /// Ask before dropping these worktree paths. Empty input does nothing.
    pub fn ask_discard_paths(&mut self, paths: Vec<String>) -> Effect {
        if paths.is_empty() {
            return Effect::None;
        }
        let selections = self.specs_for(&paths);
        if selections.is_empty() {
            return Effect::None;
        }
        let (title, message) = if paths.len() == 1 {
            (
                "Discard this file?".to_string(),
                format!(
                    "Uncommitted changes in {} are dropped. Undo can bring them back.",
                    paths[0]
                ),
            )
        } else {
            (
                format!("Discard {} files?", paths.len()),
                "The selected uncommitted changes are dropped. Undo can bring them back."
                    .to_string(),
            )
        };
        self.confirm = Some(PendingConfirm {
            title,
            message,
            confirm_label: "Discard".to_string(),
            action: Action::Discard { selections },
        });
        Effect::None
    }

    pub fn open_actions_for(&mut self, sha: &str) -> Result<(), String> {
        let sha = self.resolve_sha(sha);
        if !self.commit_exists(&sha) {
            return Err(format!("No commit {sha}"));
        }
        self.open_menu(MenuKind::CommitActions { sha });
        Ok(())
    }

    /// Context menu for one file. A multi-file mark that includes it acts on the mark.
    pub fn open_file_actions(&mut self, path: &str) -> Effect {
        let paths = if self.marked.len() > 1 && self.marked.contains(path) {
            self.marked.iter().cloned().collect()
        } else {
            vec![path.to_string()]
        };
        self.open_paths_actions(paths)
    }

    /// Context menu for a group or an explicit set of paths.
    pub fn open_paths_actions(&mut self, paths: Vec<String>) -> Effect {
        let paths = if self.marked.len() > 1 && paths.iter().any(|path| self.marked.contains(path))
        {
            self.marked.iter().cloned().collect()
        } else {
            paths
        };
        if paths.is_empty() {
            return Effect::None;
        }
        self.open_menu(MenuKind::FileActions { paths });
        Effect::None
    }

    pub fn open_hunk_actions(&mut self, path: &str, header: &str) -> Effect {
        self.open_menu(MenuKind::HunkActions {
            path: path.to_string(),
            header: header.to_string(),
        });
        Effect::None
    }

    pub fn open_lane_actions(&mut self, name: &str) -> Result<(), String> {
        self.ensure_lane(name)?;
        self.open_menu(MenuKind::LaneActions {
            name: name.to_string(),
        });
        Ok(())
    }

    pub fn open_listed_branch(&mut self, name: &str) {
        let name = name.trim().trim_start_matches("refs/heads/");
        if name.is_empty() {
            return;
        }
        self.open_menu(MenuKind::ListedBranch {
            name: name.to_string(),
        });
    }

    /// Move the open menu to a click point. The view clamps it inside the window.
    pub fn place_menu(&mut self, x: f32, y: f32) {
        if let Some(menu) = &mut self.menu {
            menu.anchor_x = x;
            menu.anchor_y = y;
        }
    }

    pub fn menu_anchor(&self) -> (f32, f32) {
        self.menu
            .as_ref()
            .map(|menu| (menu.anchor_x, menu.anchor_y))
            .unwrap_or((220.0, 168.0))
    }

    /// Squash the focused commit into its parent, with no confirm step.
    #[cfg(feature = "devtools")]
    pub fn plan_squash(&mut self) -> Result<Effect, String> {
        let Some((subject, target)) = self.squash_ends() else {
            return Err("This commit has no parent on its lane to squash into.".to_string());
        };
        Ok(Effect::Mutate(Action::Squash { subject, target }))
    }

    /// Uncommit the focused commit, with no confirm step.
    #[cfg(feature = "devtools")]
    pub fn plan_uncommit(&mut self) -> Result<Effect, String> {
        let Some(sha) = self.action_commit() else {
            return Err("Focus a commit first.".to_string());
        };
        Ok(Effect::Mutate(Action::Uncommit { sha }))
    }

    pub fn menu_cursor(&self) -> usize {
        self.menu.as_ref().map(|menu| menu.cursor).unwrap_or(0)
    }

    pub fn set_menu_cursor(&mut self, index: usize) {
        if let Some(menu) = &mut self.menu {
            menu.cursor = index;
        }
    }

    /// Where the popover should sit: `file`, `hunk`, `commit`, `branch`, or `composer`.
    #[cfg(feature = "devtools")]
    pub fn menu_place(&self) -> &'static str {
        match self.menu.as_ref().map(|menu| &menu.kind) {
            Some(MenuKind::MoveHunk { .. } | MenuKind::HunkActions { .. }) => "hunk",
            Some(MenuKind::MoveCommit { .. } | MenuKind::CommitActions { .. }) => "commit",
            Some(MenuKind::LaneActions { .. } | MenuKind::ListedBranch { .. }) => "branch",
            Some(MenuKind::ComposerLane | MenuKind::AmendInto) => "composer",
            _ => "file",
        }
    }

    pub fn menu_naming(&self) -> bool {
        self.menu.as_ref().is_some_and(|menu| menu.naming)
    }

    pub fn menu_draft(&self) -> String {
        self.menu
            .as_ref()
            .map(|menu| menu.draft.clone())
            .unwrap_or_default()
    }
}

pub fn perform_action(
    backend: &dyn Backend,
    id: &str,
    action: &Action,
) -> Result<MutationDone, String> {
    match action {
        Action::Assign { selections, lane } => {
            let workspace = backend
                .assign(id, selections, lane.as_deref())
                .map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let label = assign_label(selections, lane.as_deref());
            Ok(done(workspace, worktree, vec![label], None).reload())
        }
        Action::Discard { selections } => {
            let result = backend.discard(id, selections).map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let dropped = selections.iter().map(|spec| spec.path.clone()).collect();
            let mut done = done(
                result.workspace,
                worktree,
                vec![discard_label(selections)],
                None,
            );
            done.dropped_paths = dropped;
            done.reload_diff = true;
            if !result.refused.is_empty() {
                done.notice = Some(format!("GitButler kept {}", result.refused.join(", ")));
            }
            Ok(done)
        }
        Action::CreateLane {
            name,
            assign,
            commit,
        } => {
            let _created = backend.create_lane(id, name).map_err(bridge_err)?;
            let mut labels = vec![format!("new lane {name}")];
            let mut hook = None;
            let mut focus_sha = None;
            let mut notice = None;
            let mut dropped = Vec::new();
            if !assign.is_empty() {
                backend.assign(id, assign, Some(name)).map_err(bridge_err)?;
                labels.push(assign_label(assign, Some(name)));
            }
            if let Some(draft) = commit {
                let committed = commit_on(backend, id, name, draft)?;
                labels.push(committed.label);
                hook = committed.hook;
                focus_sha = committed.sha;
                notice = committed.notice;
                dropped = committed.dropped;
            }
            let workspace = backend.open_project(id).map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let mut done = done(workspace, worktree, labels, hook);
            done.reload_diff = focus_sha.is_none();
            done.focus_sha = focus_sha;
            done.notice = notice;
            done.dropped_paths = dropped;
            done.created_lane = Some(name.clone());
            done.clear_draft = commit.is_some();
            Ok(done)
        }
        Action::Commit {
            lane,
            selections,
            message,
        } => {
            let committed = commit_on(
                backend,
                id,
                lane,
                &CommitDraft {
                    selections: selections.clone(),
                    message: message.clone(),
                },
            )?;
            let workspace = backend.open_project(id).map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let mut done = done(workspace, worktree, vec![committed.label], committed.hook);
            done.focus_sha = committed.sha;
            done.notice = committed.notice;
            done.dropped_paths = committed.dropped;
            done.clear_draft = true;
            Ok(done)
        }
        Action::Amend { sha, selections } => {
            let result = backend.amend(id, sha, selections).map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let new_sha = result.new_sha.clone().unwrap_or_else(|| sha.clone());
            let mut done = done(
                result.workspace,
                worktree,
                vec![format!("amend {}", short7(&new_sha))],
                None,
            );
            done.focus_sha = Some(new_sha);
            done.dropped_paths = selections.iter().map(|spec| spec.path.clone()).collect();
            done.notice = rejected_notice(&result.rejected);
            Ok(done)
        }
        Action::Reword { sha, message } => {
            let result = backend.reword(id, sha, message).map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let new_sha = result.new_sha.clone().unwrap_or_else(|| sha.clone());
            let mut done = done(
                result.workspace,
                worktree,
                vec![format!("reword {}", short7(&new_sha))],
                None,
            );
            done.focus_sha = Some(new_sha);
            Ok(done)
        }
        Action::Squash { subject, target } => {
            let result = backend
                .squash(id, std::slice::from_ref(subject), target)
                .map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let new_sha = result.new_sha.clone().unwrap_or_else(|| target.clone());
            let mut done = done(
                result.workspace,
                worktree,
                vec![format!("squash {}", short7(&new_sha))],
                None,
            );
            done.focus_sha = Some(new_sha);
            Ok(done)
        }
        Action::Uncommit { sha } => {
            let result = backend
                .uncommit(id, std::slice::from_ref(sha), None)
                .map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            Ok(done(
                result.workspace,
                worktree,
                vec![format!("uncommit {}", short7(sha))],
                None,
            ))
        }
        Action::MoveCommit { sha, anchor } => {
            let result = backend
                .move_commit(id, std::slice::from_ref(sha), anchor)
                .map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let mut done = done(
                result.workspace,
                worktree,
                vec![format!("move {}", short7(sha))],
                None,
            );
            done.focus_sha = Some(sha.clone());
            Ok(done)
        }
        Action::InsertBlank { anchor } => {
            let result = backend.insert_blank(id, anchor).map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let sha = result.new_sha.clone().unwrap_or_default();
            let mut done = done(
                result.workspace,
                worktree,
                vec![format!("blank commit {}", short7(&sha))],
                None,
            );
            done.focus_sha = result.new_sha;
            Ok(done)
        }
        Action::Undo => {
            let result = backend.undo_last(id).map_err(bridge_err)?;
            let worktree = read_worktree(backend, id)?;
            let mut done = done(result.workspace, worktree, Vec::new(), None);
            done.pop_undo = true;
            done.reload_diff = true;
            Ok(done)
        }
    }
}

struct CommitOutcome {
    label: String,
    sha: Option<String>,
    hook: Option<String>,
    notice: Option<String>,
    dropped: Vec<String>,
}

fn commit_on(
    backend: &dyn Backend,
    id: &str,
    lane: &str,
    draft: &CommitDraft,
) -> Result<CommitOutcome, String> {
    let hook = hook_line(
        "Pre-commit",
        &backend
            .pre_commit_hook(id, &draft.selections)
            .map_err(bridge_err)?,
    )?;
    let mut message = draft.message.clone();
    let mut hook = Some(hook);
    match backend
        .message_hook(id, &message)
        .map_err(bridge_err)?
        .status
    {
        HookStatus::Rewritten { message: rewritten } => {
            message = rewritten;
            hook = Some("The message hook rewrote the commit message.".to_string());
        }
        HookStatus::Failed { error } => {
            return Err(format!("The message hook failed: {error}"));
        }
        HookStatus::Success => hook = Some("The message hook passed.".to_string()),
        HookStatus::NotConfigured => {}
    }
    let result = backend
        .commit_create(
            id,
            &Anchor::Lane(lane.to_string()),
            &draft.selections,
            &message,
        )
        .map_err(bridge_err)?;
    if let Ok(report) = backend.post_commit_hook(id)
        && let HookStatus::Failed { error } = report.status
    {
        hook = Some(format!(
            "Committed, then the post-commit hook failed: {error}"
        ));
    }
    let sha = result.new_sha.clone();
    let label = format!("commit {}", short7(sha.as_deref().unwrap_or("unknown")));
    Ok(CommitOutcome {
        label,
        sha,
        hook,
        notice: rejected_notice(&result.rejected),
        dropped: draft
            .selections
            .iter()
            .map(|spec| spec.path.clone())
            .collect(),
    })
}

fn done(
    workspace: WorkspaceSummary,
    worktree: WorktreeView,
    labels: Vec<String>,
    hook: Option<String>,
) -> MutationDone {
    MutationDone {
        workspace,
        worktree,
        labels,
        pop_undo: false,
        hook,
        notice: None,
        dropped_paths: Vec::new(),
        focus_sha: None,
        reload_diff: false,
        created_lane: None,
        clear_draft: false,
    }
}

impl MutationDone {
    fn reload(mut self) -> Self {
        self.reload_diff = true;
        self
    }
}

fn read_worktree(backend: &dyn Backend, id: &str) -> Result<WorktreeView, String> {
    backend.worktree(id).map_err(bridge_err)
}

fn bridge_err(error: gb_bridge::BridgeError) -> String {
    match error.hint {
        Some(hint) if !hint.is_empty() => format!("{} {hint}", error.message),
        _ => error.message,
    }
}

fn hook_line(name: &str, report: &HookReport) -> Result<String, String> {
    match &report.status {
        HookStatus::Success => Ok(format!("{name} hook passed.")),
        HookStatus::NotConfigured => {
            Ok(format!("No {name} hook.",).replace("Pre-commit", "pre-commit"))
        }
        HookStatus::Failed { error } => Err(format!("{name} hook failed: {error}")),
        HookStatus::Rewritten { .. } => Ok(format!("{name} hook rewrote the message.")),
    }
}

fn rejected_notice(rejected: &[gb_bridge::RejectedChange]) -> Option<String> {
    if rejected.is_empty() {
        return None;
    }
    Some(
        rejected
            .iter()
            .map(|item| format!("{}: {}", item.path, item.reason))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn action_op(action: &Action) -> &'static str {
    match action {
        Action::Assign { .. } => "assign",
        Action::Discard { .. } => "discard",
        Action::CreateLane {
            commit: Some(_), ..
        } => "commit",
        Action::CreateLane { .. } => "lane",
        Action::Commit { .. } => "commit",
        Action::Amend { .. } => "amend",
        Action::Reword { .. } => "reword",
        Action::Squash { .. } => "squash",
        Action::Uncommit { .. } => "uncommit",
        Action::MoveCommit { .. } => "move",
        Action::InsertBlank { .. } => "blank",
        Action::Undo => "undo",
    }
}

pub(crate) fn mutation_activity(action: &Action) -> (&'static str, Option<String>) {
    let row = action_row(action);
    let id = match action {
        Action::Assign { .. } => "assign",
        Action::Discard { .. } => "discard",
        Action::CreateLane { .. } => "create_lane",
        Action::Commit { .. } => "commit_create",
        Action::Amend { .. } => "amend",
        Action::Reword { .. } => "reword",
        Action::Squash { .. } => "squash",
        Action::Uncommit { .. } => "uncommit",
        Action::MoveCommit { .. } => "move_commit",
        Action::InsertBlank { .. } => "insert_blank",
        Action::Undo => "undo_last",
    };
    (id, row)
}

fn action_row(action: &Action) -> Option<String> {
    match action {
        Action::Assign { selections, .. } | Action::Discard { selections } => {
            selections.first().map(|spec| spec.path.clone())
        }
        Action::Amend { sha, .. }
        | Action::Reword { sha, .. }
        | Action::Squash { subject: sha, .. }
        | Action::Uncommit { sha }
        | Action::MoveCommit { sha, .. } => Some(sha.clone()),
        Action::Commit { selections, .. } => selections.first().map(|spec| spec.path.clone()),
        Action::CreateLane { commit, assign, .. } => commit
            .as_ref()
            .and_then(|draft| draft.selections.first().map(|spec| spec.path.clone()))
            .or_else(|| assign.first().map(|spec| spec.path.clone())),
        Action::InsertBlank { .. } | Action::Undo => None,
    }
}

fn assign_label(selections: &[SelectionSpec], lane: Option<&str>) -> String {
    let verb = if lane.is_some() { "assign" } else { "unassign" };
    match selections {
        [one] => format!("{verb} {}", one.path),
        many => format!("{verb} {} files", many.len()),
    }
}

fn discard_label(selections: &[SelectionSpec]) -> String {
    match selections {
        [one] => format!("discard {}", one.path),
        many => format!("discard {} files", many.len()),
    }
}

#[cfg(any(test, feature = "devtools"))]
fn normalize_lane(lane: Option<&str>) -> Option<String> {
    match lane.map(str::trim) {
        None | Some("") | Some("unassigned") | Some("-") | Some("none") => None,
        Some(name) => Some(name.to_string()),
    }
}

fn valid_lane_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

pub fn parse_hunk_header(header: &str) -> Option<HunkRef> {
    let rest = header.trim().strip_prefix("@@ ")?;
    let (old, rest) = rest.split_once(' ')?;
    let new = rest.split_once(" @@").map(|(new, _)| new).unwrap_or(rest);
    let old = old.strip_prefix('-')?;
    let new = new.strip_prefix('+')?;
    let (old_start, old_lines) = split_range(old)?;
    let (new_start, new_lines) = split_range(new)?;
    Some(HunkRef {
        old_start,
        old_lines,
        new_start,
        new_lines,
    })
}

fn split_range(text: &str) -> Option<(u32, u32)> {
    match text.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

fn whole_spec(file: &FileChange) -> SelectionSpec {
    let mut spec = SelectionSpec::whole(file.path.clone());
    spec.previous_path = file.old_path.clone();
    spec
}

fn hunk_spec(file: &FileChange, headers: &[&str]) -> Option<SelectionSpec> {
    let mut hunks = Vec::new();
    for header in headers {
        if header.is_empty() {
            return Some(whole_spec(file));
        }
        let parsed = parse_hunk_header(header)?;
        if parsed.header() != *header && !file.hunks.iter().any(|hunk| hunk.header == *header) {
            return None;
        }
        if !file.hunks.iter().any(|hunk| hunk.header == *header) {
            return None;
        }
        hunks.push(parsed);
    }
    if hunks.is_empty() {
        return None;
    }
    let mut spec = SelectionSpec::hunks(file.path.clone(), hunks);
    spec.previous_path = file.old_path.clone();
    Some(spec)
}

/// Marked files and hunks in worktree order. O(files + marked_hunks): the
/// marked hunks are grouped by path once instead of rescanned per file.
fn marked_specs(
    worktree: &WorktreeView,
    marked: &BTreeSet<String>,
    hunks: &BTreeSet<(String, String)>,
) -> Vec<SelectionSpec> {
    let mut by_path: HashMap<&str, Vec<&str>> = HashMap::with_capacity(hunks.len());
    for (path, header) in hunks {
        by_path
            .entry(path.as_str())
            .or_default()
            .push(header.as_str());
    }
    let mut specs = Vec::new();
    for file in &worktree.files {
        if marked.contains(&file.path) {
            specs.push(whole_spec(file));
            continue;
        }
        let Some(headers) = by_path.get(file.path.as_str()) else {
            continue;
        };
        if let Some(spec) = hunk_spec(file, headers) {
            specs.push(spec);
        }
    }
    specs
}

/// The linear scan `marked_specs` replaced. Kept as an equivalence reference.
#[cfg(test)]
fn marked_specs_ref(
    worktree: &WorktreeView,
    marked: &BTreeSet<String>,
    hunks: &BTreeSet<(String, String)>,
) -> Vec<SelectionSpec> {
    let mut specs = Vec::new();
    for file in &worktree.files {
        if marked.contains(&file.path) {
            specs.push(whole_spec(file));
            continue;
        }
        let headers: Vec<&str> = hunks
            .iter()
            .filter(|(path, _)| path == &file.path)
            .map(|(_, header)| header.as_str())
            .collect();
        if headers.is_empty() {
            continue;
        }
        if let Some(spec) = hunk_spec(file, &headers) {
            specs.push(spec);
        }
    }
    specs
}

fn lane_specs(worktree: &WorktreeView, lane: &str) -> Vec<SelectionSpec> {
    let mut specs = Vec::new();
    for file in &worktree.files {
        if file.hunks.is_empty() {
            continue;
        }
        let matched: Vec<&str> = file
            .hunks
            .iter()
            .filter(|hunk| hunk.lane.as_deref() == Some(lane))
            .map(|hunk| hunk.header.as_str())
            .collect();
        if matched.is_empty() {
            continue;
        }
        if matched.len() == file.hunks.len() {
            specs.push(whole_spec(file));
        } else if let Some(spec) = hunk_spec(file, &matched) {
            specs.push(spec);
        }
    }
    specs
}

fn lane_names(workspace: Option<&WorkspaceSummary>) -> Vec<String> {
    let Some(workspace) = workspace else {
        return Vec::new();
    };
    workspace
        .stacks
        .iter()
        .flat_map(|stack| stack.names().map(str::to_string))
        .collect()
}

fn short7(sha: &str) -> String {
    sha.chars().take(7).collect()
}

impl Store {
    fn commit_ready_message(&mut self) -> Option<String> {
        if let Some(reason) = self.commit_block() {
            self.mutation_error = Some(reason.clone());
            self.toast_bad = true;
            self.toast = Some(reason);
            if self.composer.open {
                self.composer.hook = self.mutation_error.clone();
            }
            return None;
        }
        Some(self.commit_message())
    }

    fn resolved_lane(&self) -> Option<String> {
        if self.composer.new_lane {
            return None;
        }
        if let Some(lane) = &self.composer.lane
            && self.lane_names().iter().any(|name| name == lane)
        {
            return Some(lane.clone());
        }
        self.lane_names().first().cloned()
    }

    pub(super) fn ensure_composer_lane(&mut self) {
        if self.composer.new_lane || self.composer.lane.is_some() {
            return;
        }
        let preferred = self
            .shown_path()
            .filter(|_| matches!(self.shown, ShownFile::Worktree { .. }))
            .or_else(|| self.uncommitted_path())
            .and_then(|path| self.file_lane(&path));
        self.composer.lane = preferred.or_else(|| self.lane_names().first().cloned());
    }

    fn file_lane(&self, path: &str) -> Option<String> {
        let file = self
            .worktree
            .as_ref()?
            .files
            .iter()
            .find(|file| file.path == path)?;
        let lanes = file.lanes();
        if lanes.len() == 1 {
            Some(lanes[0].to_string())
        } else {
            None
        }
    }

    fn specs_for(&self, paths: &[String]) -> Vec<SelectionSpec> {
        let Some(worktree) = &self.worktree else {
            return Vec::new();
        };
        paths
            .iter()
            .filter_map(|path| {
                worktree
                    .files
                    .iter()
                    .find(|file| file.path == *path)
                    .map(whole_spec)
            })
            .collect()
    }

    fn toggle_focused_mark(&mut self) {
        if self.pane != Pane::Uncommitted && self.pane != Pane::Changes {
            return;
        }
        if self.pane == Pane::Changes {
            if let Some((path, header)) = self.focused_hunk() {
                self.toggle_hunk_mark(&path, &header);
            }
            return;
        }
        if self.view == super::ViewMode::Tree
            && let Some(row) = self
                .tree_rows()
                .get(self.cursor(Pane::Uncommitted))
                .cloned()
            && row.file.is_none()
        {
            let index = self.folder_index();
            let range = index.range(&row.path);
            let all = index.paths[range.clone()]
                .iter()
                .all(|path| self.marked.contains(path));
            self.bump_marks();
            if all {
                for path in &index.paths[range.clone()] {
                    self.marked.remove(path);
                }
            } else {
                for path in &index.paths[range.clone()] {
                    self.marked.insert(path.clone());
                }
            }
            self.mark_anchor = index.paths.get(range.start).cloned();
            return;
        }
        let Some(path) = self.uncommitted_path() else {
            return;
        };
        self.mark_click(&path, false);
    }

    fn files_under(&self, folder: &str) -> Vec<String> {
        self.folder_paths_vec(folder)
    }

    fn paths_between(&self, anchor: &str, end: &str) -> Vec<String> {
        let paths = self.visible_file_paths();
        let Some(start) = paths.iter().position(|path| path == anchor) else {
            return vec![end.to_string()];
        };
        let Some(finish) = paths.iter().position(|path| path == end) else {
            return vec![end.to_string()];
        };
        let (low, high) = if start <= finish {
            (start, finish)
        } else {
            (finish, start)
        };
        paths[low..=high].to_vec()
    }

    fn visible_file_paths(&self) -> Vec<String> {
        match self.view {
            super::ViewMode::List => self
                .flat_files()
                .into_iter()
                .filter_map(|index| {
                    self.worktree
                        .as_ref()
                        .and_then(|worktree| worktree.files.get(index))
                        .map(|file| file.path.clone())
                })
                .collect(),
            super::ViewMode::Tree => self
                .tree_rows()
                .iter()
                .filter_map(|row| {
                    row.file.and_then(|index| {
                        self.worktree
                            .as_ref()
                            .and_then(|worktree| worktree.files.get(index))
                            .map(|file| file.path.clone())
                    })
                })
                .collect(),
        }
    }

    fn open_move_for_focus(&mut self) -> Effect {
        if self.pane == Pane::Changes && matches!(self.shown, ShownFile::Worktree { .. }) {
            if let Some((path, header)) = self.focused_hunk() {
                return self.open_hunk_menu(&path, &header);
            }
            if let Some(path) = self.shown_path() {
                return self.open_file_menu(&path);
            }
        }
        if self.pane == Pane::Uncommitted {
            if self.view == super::ViewMode::Tree
                && let Some(row) = self
                    .tree_rows()
                    .get(self.cursor(Pane::Uncommitted))
                    .cloned()
                && row.file.is_none()
            {
                let paths = self.files_under(&row.path);
                if paths.is_empty() {
                    return Effect::None;
                }
                self.open_menu(MenuKind::MoveFiles { paths });
                return Effect::None;
            }
            if let Some(path) = self.uncommitted_path() {
                return self.open_file_menu(&path);
            }
        }
        if let Some(sha) = self.action_commit() {
            self.open_menu(MenuKind::MoveCommit { sha });
        }
        Effect::None
    }

    fn focused_hunk(&self) -> Option<(String, String)> {
        let ShownFile::Worktree { path } = &self.shown else {
            return None;
        };
        let row = self
            .diff_rows
            .get(self.cursor(Pane::Changes))
            .or_else(|| self.hunk_row().and_then(|index| self.diff_rows.get(index)));
        let header = row
            .filter(|row| row.kind == RowKind::Hunk)
            .map(|row| row.text.clone())
            .or_else(|| {
                self.diff_rows
                    .iter()
                    .filter(|row| row.kind == RowKind::Hunk)
                    .nth(self.hunk_cursor)
                    .map(|row| row.text.clone())
            })?;
        Some((path.clone(), header))
    }

    fn request_discard(&mut self) -> Effect {
        if self.pane == Pane::Changes {
            let Some((path, header)) = self.focused_hunk() else {
                return Effect::None;
            };
            return self.ask_discard_hunk(&path, &header);
        }
        if self.pane != Pane::Uncommitted {
            return Effect::None;
        }
        self.ask_discard_paths(self.discard_paths())
    }

    fn discard_paths(&self) -> Vec<String> {
        if !self.marked.is_empty() {
            return self.marked.iter().cloned().collect();
        }
        if self.view == super::ViewMode::Tree
            && let Some(row) = self
                .tree_rows()
                .get(self.cursor(Pane::Uncommitted))
                .cloned()
            && row.file.is_none()
        {
            return self.files_under(&row.path);
        }
        self.uncommitted_path().into_iter().collect()
    }

    fn open_commit_actions(&mut self) -> Effect {
        let Some(sha) = self.action_commit() else {
            return Effect::None;
        };
        self.open_menu(MenuKind::CommitActions { sha });
        Effect::None
    }

    fn start_reword(&mut self) -> Effect {
        let Some(sha) = self.action_commit() else {
            return Effect::None;
        };
        let draft = self
            .commit
            .as_ref()
            .filter(|commit| commit.sha == sha || commit.short_sha == sha)
            .map(|commit| commit.message.clone())
            .or_else(|| self.commit_title(&sha))
            .unwrap_or_default();
        self.reword = Some(RewordDraft { sha, draft });
        self.menu = None;
        self.composer.field = None;
        Effect::None
    }

    fn request_squash(&mut self) -> Effect {
        let Some((subject, target)) = self.squash_ends() else {
            self.toast_bad = true;
            self.toast = Some("This commit has no parent on its lane to squash into.".to_string());
            self.mutation_error = self.toast.clone();
            return Effect::None;
        };
        self.confirm = Some(PendingConfirm {
            title: "Squash these commits?".to_string(),
            message: format!(
                "Squash {} into {}. Both messages are kept. Undo can restore the two commits.",
                short7(&subject),
                short7(&target)
            ),
            confirm_label: "Squash".to_string(),
            action: Action::Squash { subject, target },
        });
        Effect::None
    }

    fn request_uncommit(&mut self) -> Effect {
        let Some(sha) = self.action_commit() else {
            return Effect::None;
        };
        self.confirm = Some(PendingConfirm {
            title: "Uncommit?".to_string(),
            message: format!(
                "Uncommit {}. Its changes return to the worktree. Undo can put the commit back.",
                short7(&sha)
            ),
            confirm_label: "Uncommit".to_string(),
            action: Action::Uncommit { sha },
        });
        Effect::None
    }

    fn request_blank(&mut self) -> Effect {
        let anchor = if let Some(sha) = self.action_commit() {
            Anchor::Commit { sha, below: false }
        } else if let Some(name) = self.action_branch() {
            Anchor::Lane(name)
        } else {
            return Effect::None;
        };
        Effect::Mutate(Action::InsertBlank { anchor })
    }

    fn request_amend_focused(&mut self) -> Effect {
        let Some(sha) = self.action_commit() else {
            return Effect::None;
        };
        self.confirm_amend(sha)
    }

    fn confirm_amend(&mut self, sha: String) -> Effect {
        let selections = self.commit_specs();
        if selections.is_empty() {
            self.toast_bad = true;
            self.toast = Some("Select changes to amend into this commit.".to_string());
            self.mutation_error = self.toast.clone();
            return Effect::None;
        }
        self.confirm = Some(PendingConfirm {
            title: "Amend into this commit?".to_string(),
            message: format!(
                "Amend {} into {}. The commit message stays. Undo can restore it.",
                self.commit_summary(),
                short7(&sha)
            ),
            confirm_label: "Amend".to_string(),
            action: Action::Amend {
                sha,
                selections: (*selections).clone(),
            },
        });
        Effect::None
    }

    fn action_commit(&self) -> Option<String> {
        if let SideFocus::Commit { sha } = &self.side
            && self.commit_exists(sha)
        {
            return Some(self.resolve_sha(sha));
        }
        if matches!(self.pane, Pane::Branches | Pane::Detail)
            && let Some(sha) = self.commit_under_cursor()
        {
            return Some(sha);
        }
        if let ShownFile::Commit { sha, .. } = &self.shown {
            return Some(self.resolve_sha(sha));
        }
        None
    }

    fn action_branch(&self) -> Option<String> {
        if let SideFocus::Branch { stack, branch } = &self.side {
            return self
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.stacks.get(*stack))
                .and_then(|stack| stack.branches.get(*branch))
                .map(|branch| branch.name.clone());
        }
        if self.pane == Pane::Branches {
            let item = self.lane_rows().get(self.cursor(Pane::Branches)).copied()?;
            if item.commit.is_some() {
                return None;
            }
            return self
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.stacks.get(item.stack))
                .and_then(|stack| stack.branches.get(item.branch))
                .map(|branch| branch.name.clone());
        }
        None
    }

    fn commit_under_cursor(&self) -> Option<String> {
        match self.pane {
            Pane::Branches => {
                let item = self.lane_rows().get(self.cursor(Pane::Branches)).copied()?;
                let index = item.commit?;
                self.commit_at(item.stack, item.branch, index)
                    .map(|commit| commit.sha.clone())
            }
            Pane::Detail => match &self.side {
                SideFocus::Branch { stack, branch } => self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.stacks.get(*stack))
                    .and_then(|item| item.branches.get(*branch))
                    .and_then(|item| item.commits.get(self.cursor(Pane::Detail)))
                    .map(|commit| commit.sha.clone()),
                SideFocus::Commit { sha } => Some(self.resolve_sha(sha)),
                SideFocus::None => None,
            },
            Pane::Uncommitted | Pane::Changes => None,
        }
    }

    fn commit_title(&self, sha: &str) -> Option<String> {
        let workspace = self.workspace.as_ref()?;
        for stack in &workspace.stacks {
            for branch in &stack.branches {
                for commit in &branch.commits {
                    if commit.sha == sha || commit.short_sha == sha {
                        return Some(commit.title.clone());
                    }
                }
            }
        }
        None
    }

    fn commit_exists(&self, sha: &str) -> bool {
        self.commit_title(sha).is_some()
    }

    fn locate_commit(&self, sha: &str) -> Option<(String, usize, Vec<gb_bridge::CommitSummary>)> {
        let workspace = self.workspace.as_ref()?;
        for stack in &workspace.stacks {
            for branch in &stack.branches {
                if let Some(index) = branch
                    .commits
                    .iter()
                    .position(|commit| commit.sha == sha || commit.short_sha == sha)
                {
                    return Some((branch.name.clone(), index, branch.commits.clone()));
                }
            }
        }
        None
    }

    /// Subject is the newer commit. Target is its parent, or the other open commit.
    fn squash_ends(&self) -> Option<(String, String)> {
        let subject = self.action_commit()?;
        if let SideFocus::Commit { sha } = &self.side {
            let selected = self.resolve_sha(sha);
            if selected != subject && self.commit_exists(&selected) {
                return Some((subject, selected));
            }
        }
        let (_, index, commits) = self.locate_commit(&subject)?;
        let parent = commits.get(index + 1)?;
        Some((subject, parent.sha.clone()))
    }

    fn open_menu(&mut self, kind: MenuKind) {
        let keep_anchor = matches!(
            kind,
            MenuKind::MoveFiles { .. } | MenuKind::MoveHunk { .. } | MenuKind::MoveCommit { .. }
        );
        let (anchor_x, anchor_y) = if keep_anchor && let Some(menu) = &self.menu {
            (menu.anchor_x, menu.anchor_y)
        } else {
            default_anchor(&kind)
        };
        self.menu = Some(ActionMenu {
            kind,
            cursor: 0,
            naming: false,
            draft: String::new(),
            anchor_x,
            anchor_y,
        });
        self.confirm = None;
        self.reword = None;
        self.composer.field = None;
    }

    fn pin_menu_commit(&mut self) {
        if let Some(sha) = self.menu_sha() {
            self.side = SideFocus::Commit { sha };
        }
    }

    fn note_menu_error(&mut self, error: String) -> Effect {
        self.mutation_error = Some(error.clone());
        self.toast_bad = true;
        self.toast = Some(error);
        Effect::None
    }

    fn gated(&self, tip: String, enabled: bool) -> (String, bool) {
        match self.mutation_blocked() {
            Some(reason) => (reason.to_string(), false),
            None => (tip, enabled),
        }
    }

    fn assign_entries(&self) -> Vec<MenuEntry> {
        let mut entries = Vec::new();
        for lane in self.lane_names() {
            entries.push(MenuEntry {
                tip: format!("Move the selection to {lane}"),
                label: lane.clone(),
                enabled: true,
                destructive: false,
                kind: EntryKind::AssignLane(Some(lane)),
            });
        }
        entries.push(MenuEntry {
            label: "Unassigned".to_string(),
            tip: "Clear the lane on the selection".to_string(),
            enabled: true,
            destructive: false,
            kind: EntryKind::AssignLane(None),
        });
        entries.push(MenuEntry {
            label: "New lane…".to_string(),
            tip: "Create a lane and move the selection there".to_string(),
            enabled: true,
            destructive: false,
            kind: EntryKind::NewLane,
        });
        entries
    }

    fn move_commit_entries(&self, sha: &str) -> Vec<MenuEntry> {
        let mut entries = Vec::new();
        let place = self.locate_commit(sha);
        let (here, index, commits) = place
            .as_ref()
            .map(|(lane, index, commits)| (lane.as_str(), *index, commits.len()))
            .unwrap_or(("", 0, 0));
        entries.push(MenuEntry {
            label: "Move up (newer)".to_string(),
            tip: if index == 0 {
                "This commit is already the newest on its lane".to_string()
            } else {
                "Move this commit toward the tip of the lane".to_string()
            },
            enabled: index > 0,
            destructive: false,
            kind: EntryKind::MoveEarlier,
        });
        entries.push(MenuEntry {
            label: "Move down (older)".to_string(),
            tip: if index + 1 >= commits {
                "This commit is already the oldest on its lane".to_string()
            } else {
                "Move this commit toward the base of the lane".to_string()
            },
            enabled: index + 1 < commits,
            destructive: false,
            kind: EntryKind::MoveLater,
        });
        for lane in self.lane_names() {
            if lane == here {
                continue;
            }
            entries.push(MenuEntry {
                tip: format!("Move this commit onto {lane}"),
                label: format!("Move to {lane}"),
                enabled: true,
                destructive: false,
                kind: EntryKind::MoveToLane(lane),
            });
        }
        entries
    }

    fn commit_action_entries(&self, sha: &str) -> Vec<MenuEntry> {
        let parent = self.locate_commit(sha).and_then(|(_, index, commits)| {
            commits.get(index + 1).map(|commit| commit.sha.clone())
        });
        let locked = self.mutation_blocked().map(str::to_string);
        let gate = |tip: String, enabled: bool| match &locked {
            Some(reason) => (reason.clone(), false),
            None => (tip, enabled),
        };
        let (edit_tip, edit_on) = gate(
            "Check out this commit so you can change it, then save or abort".to_string(),
            true,
        );
        let (reword_tip, reword_on) = gate("Edit the commit message".to_string(), true);
        let (squash_tip, squash_on) = gate(
            if parent.is_some() {
                "Squash this commit into the one below it".to_string()
            } else {
                "This commit has no parent on its lane".to_string()
            },
            parent.is_some(),
        );
        let (uncommit_tip, uncommit_on) = gate(
            "Return this commit's changes to the worktree".to_string(),
            true,
        );
        let (move_tip, move_on) = gate(
            "Move this commit to another lane or reorder it".to_string(),
            true,
        );
        let (blank_tip, blank_on) = gate(
            "Insert an empty commit just newer than this one".to_string(),
            true,
        );
        let (amend_tip, amend_on) = gate(
            "Amend the selected uncommitted changes into this commit".to_string(),
            true,
        );
        let mut entries = vec![
            MenuEntry {
                label: "Edit commit".to_string(),
                tip: edit_tip,
                enabled: edit_on,
                destructive: false,
                kind: EntryKind::EditCommit,
            },
            MenuEntry {
                label: "Reword".to_string(),
                tip: reword_tip,
                enabled: reword_on,
                destructive: false,
                kind: EntryKind::Reword,
            },
            MenuEntry {
                label: "Squash into parent".to_string(),
                tip: squash_tip,
                enabled: squash_on,
                destructive: true,
                kind: EntryKind::Squash,
            },
            MenuEntry {
                label: "Uncommit".to_string(),
                tip: uncommit_tip,
                enabled: uncommit_on,
                destructive: true,
                kind: EntryKind::Uncommit,
            },
            MenuEntry {
                label: "Move…".to_string(),
                tip: move_tip,
                enabled: move_on,
                destructive: false,
                kind: EntryKind::OpenMove,
            },
            MenuEntry {
                label: "Insert blank commit".to_string(),
                tip: blank_tip,
                enabled: blank_on,
                destructive: false,
                kind: EntryKind::InsertBlank,
            },
            MenuEntry {
                label: "Amend selected changes".to_string(),
                tip: amend_tip,
                enabled: amend_on,
                destructive: true,
                kind: EntryKind::Amend,
            },
        ];
        if self.commit_is_conflicted(sha) {
            let (tip, enabled) = gate(
                "Choose ours or theirs for each conflicted file".to_string(),
                true,
            );
            entries.push(MenuEntry {
                label: "Resolve conflicts".to_string(),
                tip,
                enabled,
                destructive: false,
                kind: EntryKind::ResolveConflicts(sha.to_string()),
            });
        }
        entries
    }

    fn file_action_entries(&self, paths: &[String]) -> Vec<MenuEntry> {
        let (move_tip, move_on) =
            self.gated("Move the selection to another lane".to_string(), true);
        let has_changes = !self.specs_for(paths).is_empty();
        let (discard_tip, discard_on) = self.gated(
            if has_changes {
                "Drop these uncommitted changes. Undo can bring them back.".to_string()
            } else {
                "No uncommitted change for these files.".to_string()
            },
            has_changes,
        );
        vec![
            MenuEntry {
                label: "Move to…".to_string(),
                tip: move_tip,
                enabled: move_on,
                destructive: false,
                kind: EntryKind::OpenMoveFiles,
            },
            MenuEntry {
                label: "Discard".to_string(),
                tip: discard_tip,
                enabled: discard_on,
                destructive: true,
                kind: EntryKind::DiscardFiles,
            },
        ]
    }

    fn hunk_action_entries(&self, path: &str, header: &str) -> Vec<MenuEntry> {
        let (move_tip, move_on) = self.gated("Move this hunk to another lane".to_string(), true);
        let present = self
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.files.iter().find(|file| file.path == path))
            .and_then(|file| hunk_spec(file, &[header]))
            .is_some();
        let (discard_tip, discard_on) = self.gated(
            if present {
                "Drop this hunk from the worktree. Undo can bring it back.".to_string()
            } else {
                "This hunk is no longer in the worktree.".to_string()
            },
            present,
        );
        vec![
            MenuEntry {
                label: "Move hunk".to_string(),
                tip: move_tip,
                enabled: move_on,
                destructive: false,
                kind: EntryKind::OpenMoveHunk,
            },
            MenuEntry {
                label: "Discard hunk".to_string(),
                tip: discard_tip,
                enabled: discard_on,
                destructive: true,
                kind: EntryKind::DiscardHunk,
            },
        ]
    }

    fn lane_action_entries(&self, name: &str) -> Vec<MenuEntry> {
        let (push_tip, push_on) = self.gated("Push this lane to the remote".to_string(), true);
        let (lease_tip, lease_on) = self.gated(
            "Force push only if the remote still matches the last fetch".to_string(),
            true,
        );
        let (force_tip, force_on) = self.gated("Overwrite the remote branch".to_string(), true);
        let (publish_tip, publish_on) = self.publish_gate(name);
        let (blank_tip, blank_on) = self.gated(
            "Insert an empty commit at the tip of this lane".to_string(),
            true,
        );
        let (unapply_tip, unapply_on) =
            self.gated("Take this lane out of the workspace".to_string(), true);
        vec![
            MenuEntry {
                label: "Push".to_string(),
                tip: push_tip,
                enabled: push_on,
                destructive: false,
                kind: EntryKind::OpenPush(name.to_string()),
            },
            MenuEntry {
                label: "Force push with lease".to_string(),
                tip: lease_tip,
                enabled: lease_on,
                destructive: false,
                kind: EntryKind::ForceLease(name.to_string()),
            },
            MenuEntry {
                label: "Force push".to_string(),
                tip: force_tip,
                enabled: force_on,
                destructive: true,
                kind: EntryKind::ForcePush(name.to_string()),
            },
            MenuEntry {
                label: "Create pull request".to_string(),
                tip: publish_tip,
                enabled: publish_on,
                destructive: false,
                kind: EntryKind::Publish(name.to_string()),
            },
            MenuEntry {
                label: "Insert blank commit".to_string(),
                tip: blank_tip,
                enabled: blank_on,
                destructive: false,
                kind: EntryKind::InsertBlankOn(name.to_string()),
            },
            MenuEntry {
                label: "Unapply".to_string(),
                tip: unapply_tip,
                enabled: unapply_on,
                destructive: false,
                kind: EntryKind::UnapplyLane(name.to_string()),
            },
        ]
    }

    fn listed_branch_entries(&self, name: &str) -> Vec<MenuEntry> {
        let row = self.find_listed(name).cloned();
        let applied = row.as_ref().is_some_and(|row| row.applied);
        let remote_only = row
            .as_ref()
            .is_some_and(|row| row.place == BranchPlace::Remote);
        let (apply_tip, apply_on) = self.gated(
            if row.is_none() {
                "Choose a branch.".to_string()
            } else if applied {
                format!("{name} is already in the workspace. Unapply it instead.")
            } else {
                format!("Apply {name} to the workspace")
            },
            row.is_some() && !applied,
        );
        let (unapply_tip, unapply_on) = self.gated(
            if applied {
                format!("Take {name} out of the workspace")
            } else {
                format!("{name} is not in the workspace.")
            },
            applied,
        );
        let (delete_tip, delete_on) = self.gated(
            if applied {
                format!("Unapply {name} before deleting it. It is in the workspace.")
            } else if remote_only {
                "Only a local branch can be deleted.".to_string()
            } else if row.is_none() {
                "Choose a branch.".to_string()
            } else {
                format!("Delete the local branch {name}")
            },
            row.is_some() && !applied && !remote_only,
        );
        let (publish_tip, publish_on) = self.publish_gate(name);
        vec![
            MenuEntry {
                label: "Apply to workspace".to_string(),
                tip: apply_tip,
                enabled: apply_on,
                destructive: false,
                kind: EntryKind::ApplyBranch(name.to_string()),
            },
            MenuEntry {
                label: "Unapply".to_string(),
                tip: unapply_tip,
                enabled: unapply_on,
                destructive: false,
                kind: EntryKind::UnapplyBranch(name.to_string()),
            },
            MenuEntry {
                label: "Create pull request".to_string(),
                tip: publish_tip,
                enabled: publish_on,
                destructive: false,
                kind: EntryKind::Publish(name.to_string()),
            },
            MenuEntry {
                label: "Delete".to_string(),
                tip: delete_tip,
                enabled: delete_on,
                destructive: true,
                kind: EntryKind::DeleteBranch(name.to_string()),
            },
        ]
    }

    fn publish_gate(&self, name: &str) -> (String, bool) {
        if let Some(reason) = self.mutation_blocked() {
            return (reason.to_string(), false);
        }
        if !self.forge.connected {
            return (FORGE_SIGN_IN.to_string(), false);
        }
        if self.forge.publishing.is_some() {
            return (
                "Wait for the pull request to finish opening.".to_string(),
                false,
            );
        }
        if self.review_for(name).is_some() {
            return (format!("{name} already has a pull request."), false);
        }
        (
            "Open a draft pull request for this branch".to_string(),
            true,
        )
    }

    fn composer_lane_entries(&self) -> Vec<MenuEntry> {
        let mut entries: Vec<MenuEntry> = self
            .lane_names()
            .into_iter()
            .map(|lane| MenuEntry {
                tip: format!("Commit to {lane}"),
                label: lane.clone(),
                enabled: true,
                destructive: false,
                kind: EntryKind::PickLane(lane),
            })
            .collect();
        entries.push(MenuEntry {
            label: "New lane".to_string(),
            tip: "Commit onto a new lane".to_string(),
            enabled: true,
            destructive: false,
            kind: EntryKind::PickNewLane,
        });
        entries
    }

    fn amend_entries(&self) -> Vec<MenuEntry> {
        let Some(workspace) = &self.workspace else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for stack in &workspace.stacks {
            for branch in &stack.branches {
                for commit in &branch.commits {
                    entries.push(MenuEntry {
                        label: format!("{} {}", commit.short_sha, commit.title),
                        tip: format!("Amend the selection into {}", commit.sha),
                        enabled: true,
                        destructive: false,
                        kind: EntryKind::AmendCommit(commit.sha.clone()),
                    });
                }
            }
        }
        entries
    }

    fn activate_entry(&mut self, kind: EntryKind) -> Effect {
        match kind {
            EntryKind::AssignLane(lane) => {
                let selections = self.menu_selections();
                self.menu = None;
                if selections.is_empty() {
                    return Effect::None;
                }
                Effect::Mutate(Action::Assign { selections, lane })
            }
            EntryKind::NewLane => {
                if let Some(menu) = &mut self.menu {
                    menu.naming = true;
                    menu.draft.clear();
                }
                Effect::None
            }
            EntryKind::MoveToLane(lane) => {
                let Some(sha) = self.menu_sha() else {
                    return Effect::None;
                };
                self.menu = None;
                Effect::Mutate(Action::MoveCommit {
                    sha,
                    anchor: Anchor::Lane(lane),
                })
            }
            EntryKind::MoveEarlier => self.move_relative(true),
            EntryKind::MoveLater => self.move_relative(false),
            EntryKind::Reword => {
                self.pin_menu_commit();
                self.start_reword()
            }
            EntryKind::Squash => {
                self.pin_menu_commit();
                self.menu = None;
                self.request_squash()
            }
            EntryKind::Uncommit => {
                self.pin_menu_commit();
                self.menu = None;
                self.request_uncommit()
            }
            EntryKind::OpenMove => {
                let sha = self.menu_sha();
                self.menu = None;
                if let Some(sha) = sha {
                    self.open_menu(MenuKind::MoveCommit { sha });
                }
                Effect::None
            }
            EntryKind::InsertBlank => {
                let sha = self.menu_sha();
                self.menu = None;
                let Some(sha) = sha else {
                    return Effect::None;
                };
                Effect::Mutate(Action::InsertBlank {
                    anchor: Anchor::Commit { sha, below: false },
                })
            }
            EntryKind::Amend => {
                let sha = self.menu_sha();
                self.menu = None;
                let Some(sha) = sha else {
                    return Effect::None;
                };
                self.confirm_amend(sha)
            }
            EntryKind::EditCommit => {
                let sha = self.menu_sha();
                self.menu = None;
                let Some(sha) = sha else {
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
            EntryKind::PickLane(lane) => {
                self.composer.new_lane = false;
                self.composer.lane = Some(lane);
                self.composer.field = None;
                self.menu = None;
                Effect::None
            }
            EntryKind::PickNewLane => {
                self.composer.new_lane = true;
                self.menu = None;
                self.composer.field = Some(Field::LaneName);
                Effect::None
            }
            EntryKind::AmendCommit(sha) => {
                self.menu = None;
                self.confirm_amend(sha)
            }
            EntryKind::OpenMoveFiles => {
                let paths = self.menu_paths();
                if paths.is_empty() {
                    self.menu = None;
                    return Effect::None;
                }
                self.open_menu(MenuKind::MoveFiles { paths });
                Effect::None
            }
            EntryKind::DiscardFiles => {
                let paths = self.menu_paths();
                self.menu = None;
                self.ask_discard_paths(paths)
            }
            EntryKind::OpenMoveHunk => {
                let Some((path, header)) = self.menu_hunk() else {
                    self.menu = None;
                    return Effect::None;
                };
                self.open_menu(MenuKind::MoveHunk { path, header });
                Effect::None
            }
            EntryKind::DiscardHunk => {
                let Some((path, header)) = self.menu_hunk() else {
                    self.menu = None;
                    return Effect::None;
                };
                self.menu = None;
                self.ask_discard_hunk(&path, &header)
            }
            EntryKind::OpenPush(name) => {
                self.menu = None;
                match self.push_now(&name, ForcePush::No) {
                    Ok(effect) => effect,
                    Err(error) => self.note_menu_error(error),
                }
            }
            EntryKind::ForceLease(name) => {
                self.menu = None;
                match self.ask_force_push(&name, ForcePush::WithLease) {
                    Ok(()) => Effect::None,
                    Err(error) => self.note_menu_error(error),
                }
            }
            EntryKind::ForcePush(name) => {
                self.menu = None;
                match self.ask_force_push(&name, ForcePush::Force) {
                    Ok(()) => Effect::None,
                    Err(error) => self.note_menu_error(error),
                }
            }
            EntryKind::Publish(name) => {
                self.menu = None;
                match self.request_publish(&name) {
                    Ok(effect) => effect,
                    Err(error) => self.note_menu_error(error),
                }
            }
            EntryKind::UnapplyLane(name) => match self.unapply_workspace_lane(&name) {
                Ok(effect) => effect,
                Err(error) => self.note_menu_error(error),
            },
            EntryKind::InsertBlankOn(name) => {
                self.menu = None;
                Effect::Mutate(Action::InsertBlank {
                    anchor: Anchor::Lane(name),
                })
            }
            EntryKind::ApplyBranch(name) => {
                self.menu = None;
                match self.apply_named(&name) {
                    Ok(effect) => effect,
                    Err(error) => self.note_menu_error(error),
                }
            }
            EntryKind::UnapplyBranch(name) => {
                self.menu = None;
                match self.unapply_named(Some(&name)) {
                    Ok(effect) => effect,
                    Err(error) => self.note_menu_error(error),
                }
            }
            EntryKind::DeleteBranch(name) => {
                self.menu = None;
                match self.ask_delete_branch(Some(&name)) {
                    Ok(()) => Effect::None,
                    Err(error) => self.note_menu_error(error),
                }
            }
            EntryKind::ResolveConflicts(sha) => {
                self.menu = None;
                match self.open_conflicts(Some(&sha)) {
                    Ok(effect) => effect,
                    Err(error) => self.note_menu_error(error),
                }
            }
        }
    }

    fn menu_paths(&self) -> Vec<String> {
        match self.menu.as_ref().map(|menu| &menu.kind) {
            Some(MenuKind::FileActions { paths } | MenuKind::MoveFiles { paths }) => paths.clone(),
            _ => Vec::new(),
        }
    }

    fn menu_hunk(&self) -> Option<(String, String)> {
        match self.menu.as_ref().map(|menu| &menu.kind) {
            Some(MenuKind::HunkActions { path, header } | MenuKind::MoveHunk { path, header }) => {
                Some((path.clone(), header.clone()))
            }
            _ => None,
        }
    }

    fn menu_selections(&self) -> Vec<SelectionSpec> {
        let Some(menu) = &self.menu else {
            return Vec::new();
        };
        let Some(worktree) = &self.worktree else {
            return Vec::new();
        };
        match &menu.kind {
            MenuKind::MoveFiles { paths } => paths
                .iter()
                .filter_map(|path| {
                    worktree
                        .files
                        .iter()
                        .find(|file| file.path == *path)
                        .map(whole_spec)
                })
                .collect(),
            MenuKind::MoveHunk { path, header } => worktree
                .files
                .iter()
                .find(|file| file.path == *path)
                .and_then(|file| hunk_spec(file, &[header.as_str()]))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn menu_sha(&self) -> Option<String> {
        match self.menu.as_ref().map(|menu| &menu.kind) {
            Some(MenuKind::MoveCommit { sha } | MenuKind::CommitActions { sha }) => {
                Some(sha.clone())
            }
            _ => None,
        }
    }

    fn move_relative(&mut self, earlier: bool) -> Effect {
        let Some(sha) = self.menu_sha() else {
            return Effect::None;
        };
        let Some((_, index, commits)) = self.locate_commit(&sha) else {
            return Effect::None;
        };
        let anchor = if earlier {
            let Some(above) = commits.get(index.wrapping_sub(1)) else {
                return Effect::None;
            };
            Anchor::Commit {
                sha: above.sha.clone(),
                below: false,
            }
        } else {
            let Some(below) = commits.get(index + 1) else {
                return Effect::None;
            };
            Anchor::Commit {
                sha: below.sha.clone(),
                below: true,
            }
        };
        self.menu = None;
        Effect::Mutate(Action::MoveCommit { sha, anchor })
    }

    fn finish_new_lane(&mut self) -> Effect {
        let name = self
            .menu
            .as_ref()
            .map(|menu| menu.draft.trim().to_string())
            .unwrap_or_default();
        if !valid_lane_name(&name) {
            self.toast_bad = true;
            self.toast = Some("Type a lane name with no spaces.".to_string());
            self.mutation_error = self.toast.clone();
            return Effect::None;
        }
        let selections = self.menu_selections();
        self.menu = None;
        Effect::Mutate(Action::CreateLane {
            name,
            assign: selections,
            commit: None,
        })
    }

    fn apply_confirm_key(&mut self, key: &str) -> Effect {
        match key {
            "enter" => self.confirm_yes(),
            "escape" => {
                self.confirm = None;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn apply_menu_key(&mut self, key: &str) -> Effect {
        let len = self.menu_entries().len();
        match key {
            "up" => {
                if let Some(menu) = &mut self.menu
                    && menu.cursor > 0
                {
                    menu.cursor -= 1;
                }
                Effect::None
            }
            "down" => {
                if let Some(menu) = &mut self.menu
                    && len > 0
                    && menu.cursor + 1 < len
                {
                    menu.cursor += 1;
                }
                Effect::None
            }
            "enter" => self.choose_menu(),
            "escape" => {
                self.menu = None;
                Effect::None
            }
            "m" => {
                self.menu = None;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn apply_text_key(&mut self, key: &str) -> Effect {
        if self.menu.as_ref().is_some_and(|menu| menu.naming) {
            return self.type_into_menu(key);
        }
        if self.reword.is_some() {
            return self.type_into_reword(key);
        }
        match self.composer.field {
            Some(Field::Title) => self.type_title(key),
            Some(Field::Body) => self.type_body(key),
            Some(Field::LaneName) => self.type_lane(key),
            None => Effect::None,
        }
    }

    fn type_title(&mut self, key: &str) -> Effect {
        match key {
            "enter" | "tab" => self.composer.field = Some(Field::Body),
            "escape" => self.composer.field = None,
            other => {
                edit_text(&mut self.composer.title, other);
            }
        }
        Effect::None
    }

    fn type_body(&mut self, key: &str) -> Effect {
        match key {
            "enter" => self.composer.body.push('\n'),
            "tab" => self.composer.field = Some(Field::Title),
            "escape" => self.composer.field = None,
            other => {
                edit_text(&mut self.composer.body, other);
            }
        }
        Effect::None
    }

    fn type_lane(&mut self, key: &str) -> Effect {
        match key {
            "enter" | "escape" => self.composer.field = None,
            "tab" => self.composer.field = Some(Field::Title),
            other => edit_text(&mut self.composer.lane_name, other),
        }
        Effect::None
    }

    fn type_into_menu(&mut self, key: &str) -> Effect {
        match key {
            "enter" => self.finish_new_lane(),
            "escape" => {
                if let Some(menu) = &mut self.menu {
                    menu.naming = false;
                }
                Effect::None
            }
            other => {
                if let Some(menu) = &mut self.menu {
                    edit_text(&mut menu.draft, other);
                }
                Effect::None
            }
        }
    }

    fn type_into_reword(&mut self, key: &str) -> Effect {
        match key {
            "enter" => {
                let Some(draft) = self.reword.clone() else {
                    return Effect::None;
                };
                if draft.draft.trim().is_empty() {
                    self.toast_bad = true;
                    self.toast = Some("Write a commit message first.".to_string());
                    return Effect::None;
                }
                self.reword = None;
                Effect::Mutate(Action::Reword {
                    sha: draft.sha,
                    message: draft.draft,
                })
            }
            "escape" => {
                self.reword = None;
                Effect::None
            }
            other => {
                if let Some(draft) = &mut self.reword {
                    edit_text(&mut draft.draft, other);
                }
                Effect::None
            }
        }
    }
}

fn default_anchor(kind: &MenuKind) -> (f32, f32) {
    match kind {
        MenuKind::MoveCommit { .. } | MenuKind::CommitActions { .. } => (760.0, 168.0),
        MenuKind::LaneActions { .. } => (860.0, 150.0),
        MenuKind::ListedBranch { .. } => (280.0, 180.0),
        MenuKind::MoveHunk { .. } | MenuKind::HunkActions { .. } => (420.0, 180.0),
        MenuKind::ComposerLane | MenuKind::AmendInto => (16.0, 320.0),
        MenuKind::MoveFiles { .. } | MenuKind::FileActions { .. } => (220.0, 168.0),
    }
}

fn edit_text(buffer: &mut String, key: &str) {
    match key {
        "backspace" => {
            buffer.pop();
        }
        "space" => buffer.push(' '),
        "enter" | "escape" | "tab" | "up" | "down" | "left" | "right" => {}
        other if other.chars().count() == 1 && !other.chars().any(char::is_control) => {
            buffer.push_str(other);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AppPage, EditPhase, Loaded, Store, ViewMode, load};
    use gb_bridge::{AssignedHunk, BranchSummary, FakeBackend, FileStatus, StackSummary};

    fn ready() -> (FakeBackend, Store) {
        let backend = FakeBackend::demo();
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        (backend, store)
    }

    fn run(backend: &FakeBackend, store: &mut Store, effect: Effect) -> MutationDone {
        let Effect::Mutate(action) = effect else {
            panic!("expected a mutation, got {effect:?}");
        };
        assert!(store.arm_busy(&action));
        let done = perform_action(backend, "demo-project", &action).expect("mutation");
        let follow = store.accept(done.clone());
        let _ = follow;
        done
    }

    fn file<'a>(store: &'a Store, path: &str) -> &'a FileChange {
        store
            .worktree
            .as_ref()
            .unwrap()
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("missing {path}"))
    }

    #[test]
    fn hunk_headers_round_trip_into_selection_specs() {
        let header = "@@ -1,4 +1,154 @@";
        let hunk = parse_hunk_header(header).unwrap();
        assert_eq!(hunk.header(), header);
        let (_backend, store) = ready();
        let split = file(&store, "src/split.rs");
        let spec = hunk_spec(split, &["@@ -1,2 +1,4 @@"]).unwrap();
        assert_eq!(spec.path, "src/split.rs");
        assert_eq!(spec.hunks.as_ref().unwrap().len(), 1);
        let whole = whole_spec(split);
        assert!(whole.hunks.is_none());
        let binary = file(&store, "assets/logo.bin");
        let spec = hunk_spec(binary, &[""]).unwrap();
        assert!(spec.hunks.is_none());
    }

    #[test]
    fn marks_drive_bulk_move_and_survive_the_tree() {
        let (_backend, mut store) = ready();
        store.pane = Pane::Uncommitted;
        let first = store.uncommitted_path().unwrap();
        store.apply_key("space");
        assert!(store.file_marked(&first));
        store.apply_key("down");
        let second = store.uncommitted_path().unwrap();
        store.mark_click(&second, true);
        assert!(store.file_marked(&first) && store.file_marked(&second));
        let effect = store.apply_key("m");
        assert!(matches!(effect, Effect::None));
        let MenuKind::MoveFiles { paths } = store.menu.as_ref().unwrap().kind.clone() else {
            panic!("file menu");
        };
        assert!(paths.contains(&first) && paths.contains(&second));
        store.apply_key("escape");
        assert!(store.menu.is_none());
        store.set_view(ViewMode::Tree);
        assert!(store.file_marked(&first));
        store.focus_file("src/app.rs");
        store.pane = Pane::Uncommitted;
        for _ in 0..12 {
            let on_src = store
                .tree_rows()
                .get(store.cursor(Pane::Uncommitted))
                .is_some_and(|row| row.path == "src");
            if on_src {
                break;
            }
            store.apply_key("up");
        }
        assert_eq!(
            store.tree_rows()[store.cursor(Pane::Uncommitted)].path,
            "src"
        );
        store.apply_key("space");
        assert!(store.file_marked("src/app.rs"));
        assert!(store.file_marked("src/split.rs"));
    }

    #[test]
    fn the_composer_explains_why_commit_is_disabled() {
        let (_backend, mut store) = ready();
        store.set_composer_open(true);
        assert_eq!(
            store.commit_block().as_deref(),
            Some("Type a commit title.")
        );
        store.composer.title = "Show the lane name".to_string();
        store.composer.lane = Some("lane-a".to_string());
        assert!(store.commit_block().is_none());
        assert_eq!(store.commit_summary(), "3 files, 1 hunk");
        let specs = store.commit_specs();
        assert!(
            specs
                .iter()
                .any(|spec| spec.path == "src/app.rs" && spec.hunks.is_none())
        );
        let split = specs
            .iter()
            .find(|spec| spec.path == "src/split.rs")
            .unwrap();
        assert_eq!(split.hunks.as_ref().unwrap().len(), 1);
        store.mark_click("README.md", false);
        let specs = store.commit_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].path, "README.md");
        store.marked.clear();
        Arc::make_mut(store.workspace.as_mut().unwrap())
            .stacks
            .push(StackSummary {
                branches: vec![BranchSummary {
                    name: "empty-lane".to_string(),
                    tip_sha: String::new(),
                    tip_full: String::new(),
                    commits: Vec::new(),
                }],
            });
        store.composer.lane = Some("empty-lane".to_string());
        assert_eq!(
            store.commit_block().as_deref(),
            Some("Nothing is selected for this commit.")
        );
        store.composer.title = "x".repeat(51);
        store.composer.body = format!("{}\nshort", "y".repeat(73));
        assert!(store.title_over());
        assert!(store.body_over());
        assert!(store.ruler_hint().contains("51/50"));
        assert!(store.ruler_hint().contains("73/72"));
        store.composer.new_lane = true;
        store.composer.lane_name = "has space".to_string();
        store.composer.title = "Title".to_string();
        store.mark_click("README.md", false);
        assert_eq!(
            store.commit_block().as_deref(),
            Some("Type a lane name with no spaces.")
        );
    }

    #[test]
    fn menu_keys_plan_an_assign_and_escape_does_not() {
        let (backend, mut store) = ready();
        store.focus_file("src/util.rs");
        store.pane = Pane::Uncommitted;
        assert_eq!(store.apply_key("m"), Effect::None);
        assert!(store.menu.is_some());
        let before = file(&store, "src/util.rs").assignment_label();
        assert_eq!(before, "Unassigned");
        store.apply_key("escape");
        assert!(store.menu.is_none());
        assert_eq!(file(&store, "src/util.rs").assignment_label(), "Unassigned");
        store.apply_key("m");
        let entries = store.menu_entries();
        let index = entries
            .iter()
            .position(|entry| entry.label == "lane-a")
            .unwrap();
        store.menu.as_mut().unwrap().cursor = index;
        let effect = store.apply_key("enter");
        let done = run(&backend, &mut store, effect);
        assert_eq!(done.labels, vec!["assign src/util.rs".to_string()]);
        assert_eq!(
            file(&store, "src/util.rs").hunks[0].lane.as_deref(),
            Some("lane-a")
        );
        assert_eq!(
            store.undo_caption().as_deref(),
            Some("Undo: assign src/util.rs")
        );
    }

    #[test]
    fn busy_and_errors_stay_on_the_row_until_the_next_try() {
        let (_backend, mut store) = ready();
        let action = Action::Undo;
        assert!(store.arm_busy(&action));
        assert!(!store.arm_busy(&action));
        assert_eq!(store.busy.as_ref().unwrap().op, "undo");
        store.fail_busy("GitButler could not undo that.");
        assert!(store.busy.is_none());
        assert_eq!(
            store.mutation_error.as_deref(),
            Some("GitButler could not undo that.")
        );
        assert_eq!(store.toast, store.mutation_error);
        store.composer.open = true;
        store.arm_busy(&Action::Commit {
            lane: "lane-a".to_string(),
            selections: vec![SelectionSpec::whole("README.md")],
            message: "Title".to_string(),
        });
        assert!(store.row_busy("README.md"));
        store.fail_busy("nope");
        assert_eq!(store.composer.hook.as_deref(), Some("nope"));
    }

    #[test]
    fn discard_waits_for_confirm_and_clears_a_deleted_file() {
        let (backend, mut store) = ready();
        store.focus_file("src/retired.rs");
        store.pane = Pane::Uncommitted;
        assert_eq!(store.apply_key("d"), Effect::None);
        let confirm = store.confirm.clone().unwrap();
        assert_eq!(confirm.confirm_label, "Discard");
        assert!(matches!(store.shown, ShownFile::Worktree { .. }));
        let effect = store.confirm_yes();
        run(&backend, &mut store, effect);
        assert!(
            store
                .worktree
                .as_ref()
                .unwrap()
                .files
                .iter()
                .all(|file| file.path != "src/retired.rs")
        );
        assert!(matches!(store.shown, ShownFile::None));
        assert!(!store.file_marked("src/retired.rs"));
        assert!(store.undo_caption().unwrap().starts_with("Undo: discard"));
    }

    #[test]
    fn assign_commit_reword_squash_uncommit_and_undo() {
        let (backend, mut store) = ready();
        let effect = store
            .assign_paths(&["src/util.rs".to_string()], Some("lane-a"))
            .unwrap();
        run(&backend, &mut store, effect);
        assert_eq!(
            file(&store, "src/util.rs").hunks[0].lane.as_deref(),
            Some("lane-a")
        );

        store.marked.clear();
        store.marked_hunks.clear();
        let effect = store
            .commit_with(
                "Note the util helper",
                "Body for the helper.",
                Some("lane-a"),
            )
            .unwrap();
        let committed = run(&backend, &mut store, effect);
        let sha = committed.focus_sha.clone().unwrap();
        assert!(store.commit_exists(&sha));
        assert!(matches!(store.side, SideFocus::Commit { .. }));
        assert!(
            store
                .worktree
                .as_ref()
                .unwrap()
                .files
                .iter()
                .all(|file| file.path != "src/util.rs")
        );
        assert_eq!(store.composer.title, "");
        assert_eq!(store.composer.hook.as_deref(), Some("No pre-commit hook."));
        assert!(store.undo_caption().unwrap().starts_with("Undo: commit "));

        let effect = store.reword_to(&sha, "Noted the util helper").unwrap();
        let reworded = run(&backend, &mut store, effect);
        let sha = reworded.focus_sha.clone().unwrap();
        assert_eq!(
            store.commit_title(&sha).as_deref(),
            Some("Noted the util helper")
        );
        assert!(store.undo_caption().unwrap().starts_with("Undo: reword "));

        let parent_title = {
            let (_, index, commits) = store.locate_commit(&sha).unwrap();
            commits[index + 1].title.clone()
        };
        assert_eq!(parent_title, "Wire the lane header");
        store.side = SideFocus::Commit { sha: sha.clone() };
        let effect = store.request_squash();
        assert!(store.confirm.is_some());
        let effect = if matches!(effect, Effect::None) {
            store.confirm_yes()
        } else {
            effect
        };
        let squashed = run(&backend, &mut store, effect);
        let sha = squashed.focus_sha.clone().unwrap();
        assert_eq!(
            store.commit_title(&sha).as_deref(),
            Some("Wire the lane header")
        );
        assert!(store.undo_caption().unwrap().starts_with("Undo: squash "));

        let before = store
            .workspace
            .as_ref()
            .unwrap()
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-a")
            .unwrap()
            .commits
            .len();
        let effect = store.uncommit_sha(&sha).unwrap();
        run(&backend, &mut store, effect);
        let after = store
            .workspace
            .as_ref()
            .unwrap()
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-a")
            .unwrap()
            .commits
            .len();
        assert!(after < before);
        assert!(!store.commit_exists(&sha));
        assert!(store.undo_caption().unwrap().starts_with("Undo: uncommit "));

        let effect = store.request_undo();
        run(&backend, &mut store, effect);
        let restored = store
            .workspace
            .as_ref()
            .unwrap()
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-a")
            .unwrap()
            .commits
            .len();
        assert_eq!(restored, before);
        assert!(store.undo_caption().unwrap().starts_with("Undo: squash "));
    }

    #[test]
    fn blank_commit_move_and_new_lane_round_trip() {
        let (backend, mut store) = ready();
        let sha = store.commit_sha_at(0).unwrap();
        store.focus_commit(&sha);
        let effect = store.apply_key("i");
        let inserted = run(&backend, &mut store, effect);
        let blank = inserted.focus_sha.clone().unwrap();
        assert_eq!(
            store.commit_title(&blank).as_deref(),
            Some("(empty commit)")
        );
        assert!(
            store
                .undo_caption()
                .unwrap()
                .starts_with("Undo: blank commit ")
        );

        store.side = SideFocus::Commit { sha: blank.clone() };
        store.pane = Pane::Branches;
        store.apply_key("m");
        assert!(matches!(
            store.menu.as_ref().map(|menu| &menu.kind),
            Some(MenuKind::MoveCommit { .. })
        ));
        let entries = store.menu_entries();
        let index = entries
            .iter()
            .position(|entry| entry.label == "Move to lane-b")
            .unwrap();
        store.menu.as_mut().unwrap().cursor = index;
        let effect = store.apply_key("enter");
        run(&backend, &mut store, effect);
        let (_, _, commits) = store.locate_commit(&blank).unwrap();
        let on_b = store
            .workspace
            .as_ref()
            .unwrap()
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-b")
            .unwrap()
            .commits
            .iter()
            .any(|commit| commit.sha == blank);
        assert!(on_b, "{commits:?}");
    }

    #[test]
    fn typing_a_title_does_not_move_the_file_list() {
        let (_backend, mut store) = ready();
        store.focus_field(Field::Title);
        let cursor = store.cursor(Pane::Uncommitted);
        store.apply_key("n");
        store.apply_key("down");
        store.apply_key("space");
        assert_eq!(store.composer.title, "n ");
        store.apply_key("backspace");
        assert_eq!(store.composer.title, "n");
        assert_eq!(store.cursor(Pane::Uncommitted), cursor);
        store.apply_key("enter");
        assert_eq!(store.composer.field, Some(Field::Body));
        store.apply_key("escape");
        assert!(store.composer.field.is_none());
        assert!(store.composer.open);
        store.apply_key("escape");
        assert!(!store.composer.open);
    }

    #[test]
    fn a_failed_hook_does_not_commit() {
        struct HookFail(FakeBackend);
        impl gb_bridge::Settings for HookFail {}
        impl Backend for HookFail {
            fn environment(&self) -> gb_bridge::Result<gb_bridge::Environment> {
                self.0.environment()
            }
            fn list_projects(&self) -> gb_bridge::Result<gb_bridge::ProjectList> {
                self.0.list_projects()
            }
            fn open_project(&self, id: &str) -> gb_bridge::Result<WorkspaceSummary> {
                self.0.open_project(id)
            }
            fn worktree(&self, id: &str) -> gb_bridge::Result<WorktreeView> {
                self.0.worktree(id)
            }
            fn watch(
                &self,
                id: &str,
                on_change: Box<dyn Fn(gb_bridge::BridgeEvent) + Send + Sync>,
            ) -> gb_bridge::Result<gb_bridge::WatchHandle> {
                self.0.watch(id, on_change)
            }
            fn pre_commit_hook(
                &self,
                _id: &str,
                _selections: &[SelectionSpec],
            ) -> gb_bridge::Result<HookReport> {
                Ok(HookReport {
                    status: HookStatus::Failed {
                        error: "names must be short".to_string(),
                    },
                })
            }
        }
        let backend = HookFail(FakeBackend::demo());
        let before = backend.open_project("demo-project").unwrap();
        let specs = vec![SelectionSpec::whole("README.md")];
        let error = perform_action(
            &backend,
            "demo-project",
            &Action::Commit {
                lane: "lane-a".to_string(),
                selections: specs,
                message: "Title".to_string(),
            },
        )
        .unwrap_err();
        assert!(error.contains("names must be short"), "{error}");
        let after = backend.open_project("demo-project").unwrap();
        assert_eq!(
            before.stacks[0].commit_count(),
            after.stacks[0].commit_count()
        );
    }

    #[test]
    fn reload_keeps_the_undo_label() {
        let (backend, mut store) = ready();
        let effect = store
            .assign_paths(&["README.md".to_string()], Some("lane-b"))
            .unwrap();
        let done = run(&backend, &mut store, effect);
        let loaded = Loaded {
            env: store.env.clone().unwrap(),
            projects: store.projects.clone(),
            list_reason: None,
            workspace: Some(done.workspace),
            worktree: Some(done.worktree),
            forge: gb_bridge::ForgeStatus {
                connected: store.forge.connected,
                detail: store.forge.detail.clone(),
                reviews: store.forge.reviews.clone(),
            },
            error: None,
        };
        store.apply(loaded);
        assert_eq!(
            store.undo_caption().as_deref(),
            Some("Undo: assign README.md")
        );
    }

    #[test]
    fn file_menu_marks_discard_and_locks_while_editing() {
        let (_backend, mut store) = ready();
        store.open_file_actions("src/app.rs");
        let entries = store.menu_entries();
        let move_to = entries
            .iter()
            .find(|entry| entry.label == "Move to…")
            .unwrap();
        assert!(move_to.enabled && !move_to.destructive);
        let discard = entries
            .iter()
            .find(|entry| entry.label == "Discard")
            .unwrap();
        assert!(discard.destructive && discard.enabled && !discard.tip.is_empty());
        store.pages.phase = EditPhase::Editing;
        store.open_file_actions("src/app.rs");
        for entry in store.menu_entries() {
            assert!(!entry.enabled, "{}", entry.label);
            assert!(!entry.tip.is_empty(), "{}", entry.label);
        }
    }

    #[test]
    fn commit_menu_explains_squash_and_offers_resolve() {
        let (_backend, mut store) = ready();
        let oldest = store
            .workspace
            .as_ref()
            .unwrap()
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .flat_map(|branch| branch.commits.last())
            .find(|commit| commit.sha.contains("a00a00a") || commit.short_sha == "a00a00a")
            .map(|commit| commit.sha.clone())
            .unwrap_or_else(|| {
                store
                    .workspace
                    .as_ref()
                    .unwrap()
                    .stacks
                    .iter()
                    .flat_map(|stack| &stack.branches)
                    .filter_map(|branch| branch.commits.last().map(|commit| commit.sha.clone()))
                    .next()
                    .unwrap()
            });
        store.open_actions_for(&oldest).unwrap();
        let squash = store
            .menu_entries()
            .into_iter()
            .find(|entry| entry.label == "Squash into parent")
            .unwrap();
        assert!(squash.destructive);
        assert!(!squash.enabled);
        assert!(squash.tip.contains("no parent"));

        let tip = store.commit_sha_at(0).unwrap();
        store.open_actions_for(&tip).unwrap();
        let squash = store
            .menu_entries()
            .into_iter()
            .find(|entry| entry.label == "Squash into parent")
            .unwrap();
        assert!(squash.enabled && squash.destructive);

        let conflicted = store
            .workspace
            .as_ref()
            .unwrap()
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .flat_map(|branch| &branch.commits)
            .find(|commit| commit.conflicted)
            .map(|commit| commit.sha.clone())
            .unwrap();
        store.open_actions_for(&conflicted).unwrap();
        assert!(
            store
                .menu_entries()
                .iter()
                .any(|entry| entry.label == "Resolve conflicts" && entry.enabled)
        );
    }

    #[test]
    fn lane_menu_marks_force_push_and_explains_an_existing_review() {
        let (_backend, mut store) = ready();
        store.open_lane_actions("lane-a").unwrap();
        let entries = store.menu_entries();
        let push = entries.iter().find(|entry| entry.label == "Push").unwrap();
        assert!(push.enabled);
        let force = entries
            .iter()
            .find(|entry| entry.label == "Force push")
            .unwrap();
        assert!(force.destructive && force.enabled);
        let publish = entries
            .iter()
            .find(|entry| entry.label == "Create pull request")
            .unwrap();
        assert!(!publish.enabled);
        assert!(publish.tip.contains("already"));
        store.open_lane_actions("lane-c").unwrap();
        let publish = store
            .menu_entries()
            .into_iter()
            .find(|entry| entry.label == "Create pull request")
            .unwrap();
        assert!(publish.enabled);
    }

    #[test]
    fn menu_arrows_and_escape_stay_on_the_branches_page() {
        let (_backend, mut store) = ready();
        store.show_page(AppPage::Branches);
        store.open_file_actions("src/app.rs");
        assert_eq!(store.menu_cursor(), 0);
        store.apply_key("down");
        assert_eq!(store.menu_cursor(), 1);
        store.apply_key("up");
        assert_eq!(store.menu_cursor(), 0);
        store.apply_key("escape");
        assert!(store.menu.is_none());
        assert_eq!(store.app_page(), AppPage::Branches);
    }

    type MarkPair = (String, String);

    fn hunky(path: &str, hunks: &[(&str, Option<&str>)]) -> FileChange {
        FileChange {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            hunks: hunks
                .iter()
                .map(|(header, lane)| AssignedHunk {
                    header: (*header).to_string(),
                    lane: lane.map(|lane| lane.to_string()),
                })
                .collect(),
        }
    }

    #[test]
    fn marked_specs_matches_the_linear_reference() {
        let empty = WorktreeView {
            files: Vec::new(),
            changed_paths: Vec::new(),
        };
        let worktree = WorktreeView {
            files: vec![
                hunky("src/a.rs", &[("@@ -1,2 +1,4 @@", Some("lane-a"))]),
                hunky(
                    "src/deep/b.rs",
                    &[
                        ("@@ -1,1 +1,2 @@", Some("lane-b")),
                        ("@@ -5,1 +5,2 @@", Some("lane-a")),
                    ],
                ),
                hunky("README.md", &[]),
                hunky("whole.rs", &[("", Some("lane-a"))]),
            ],
            changed_paths: Vec::new(),
        };
        let header_a = "@@ -1,1 +1,2 @@";
        let header_b = "@@ -5,1 +5,2 @@";
        let cases: Vec<(BTreeSet<String>, BTreeSet<MarkPair>)> = vec![
            (BTreeSet::new(), BTreeSet::new()),
            (
                ["src/a.rs".to_string(), "README.md".to_string()]
                    .into_iter()
                    .collect(),
                BTreeSet::new(),
            ),
            (
                BTreeSet::new(),
                [("src/deep/b.rs".to_string(), header_a.to_string())]
                    .into_iter()
                    .collect(),
            ),
            (
                BTreeSet::new(),
                [
                    ("src/deep/b.rs".to_string(), header_a.to_string()),
                    ("src/deep/b.rs".to_string(), header_b.to_string()),
                    ("whole.rs".to_string(), String::new()),
                    ("missing.rs".to_string(), header_a.to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            (
                ["src/deep/b.rs".to_string()].into_iter().collect(),
                [("src/a.rs".to_string(), header_a.to_string())]
                    .into_iter()
                    .collect(),
            ),
            (
                ["src/a.rs".to_string(), "src/deep/b.rs".to_string()]
                    .into_iter()
                    .collect(),
                [("whole.rs".to_string(), String::new())]
                    .into_iter()
                    .collect(),
            ),
        ];
        for (marked, hunks) in &cases {
            assert_eq!(
                marked_specs(&worktree, marked, hunks),
                marked_specs_ref(&worktree, marked, hunks),
                "{marked:?} {hunks:?}"
            );
            assert_eq!(
                marked_specs(&empty, marked, hunks),
                marked_specs_ref(&empty, marked, hunks),
            );
        }
    }

    #[test]
    fn commit_specs_match_the_uncached_selectors_and_refresh_after_marks() {
        let (_backend, mut store) = ready();
        store.composer.lane = Some("lane-a".to_string());
        let worktree = store.worktree.clone().unwrap();
        let lane = "lane-a".to_string();

        let lane_cached = store.commit_specs();
        assert_eq!(*lane_cached, lane_specs(&worktree, &lane));

        let split_header = file(&store, "src/split.rs").hunks[0].header.clone();
        store.toggle_hunk_mark("src/split.rs", &split_header);
        let hunk_cached = store.commit_specs();
        let hunks = BTreeSet::from([("src/split.rs".to_string(), split_header.clone())]);
        assert_eq!(*hunk_cached, marked_specs(&worktree, &store.marked, &hunks));

        store.mark_click("src/app.rs", false);
        let marked_cached = store.commit_specs();
        assert_eq!(
            *marked_cached,
            marked_specs(&worktree, &store.marked, &store.marked_hunks)
        );

        // Unmarking everything returns to the lane selection without staleness.
        store.mark_click("src/app.rs", false);
        store.toggle_hunk_mark("src/split.rs", &split_header);
        assert_eq!(*store.commit_specs(), *lane_cached);

        // A different composer lane changes the result too.
        store.composer.lane = Some("lane-b".to_string());
        assert_eq!(*store.commit_specs(), lane_specs(&worktree, "lane-b"));
    }

    #[test]
    fn hunk_marked_probes_the_marked_set_without_file_bleed() {
        let (_backend, mut store) = ready();
        let split_header = file(&store, "src/split.rs").hunks[0].header.clone();
        let other_header = file(&store, "src/split.rs")
            .hunks
            .get(1)
            .map(|hunk| hunk.header.clone());
        assert!(!store.hunk_marked("src/split.rs", &split_header));
        store.toggle_hunk_mark("src/split.rs", &split_header);
        assert!(store.hunk_marked("src/split.rs", &split_header));
        assert!(!store.hunk_marked("src/split.rs", "@@ -99,1 +99,1 @@"));
        if let Some(other_header) = &other_header {
            assert!(!store.hunk_marked("src/split.rs", other_header));
        }
        // A file mark never marks its hunks.
        store.mark_click("src/app.rs", false);
        assert!(!store.hunk_marked("src/app.rs", "any"));
        // Toggling again clears it, and the index follows.
        store.toggle_hunk_mark("src/split.rs", &split_header);
        assert!(!store.hunk_marked("src/split.rs", &split_header));
    }
}
