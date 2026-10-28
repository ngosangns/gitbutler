//! One registry for every bridge call the window can show.
//!
//! The phase is the truth. Spinners wait out a slow-start so a fast call does
//! not flicker, then stay up for a minimum so they do not flash. Tests pass a
//! frozen clock. The window uses wall time.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Wait this long before a spinner appears.
pub const SLOW_START_MS: u64 = 150;
/// Once a spinner is showing, keep it at least this long.
pub const MIN_VISIBLE_MS: u64 = 400;
/// How long a success stays on the strip and in a toast.
pub const RESULT_HOLD_MS: u64 = 4_000;

/// Where a busy operation draws a skeleton.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// Uncommitted, the diff, and the branches column together.
    Workspace,
    Uncommitted,
    Diff,
    BranchesColumn,
    BranchesPage,
    HistoryPage,
    /// A button, a row, or a toast. The columns stay as they are.
    Global,
}

/// Phase stored for one operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpState {
    Idle,
    Loading {
        started_at: u64,
        label: String,
    },
    Success {
        message: String,
    },
    Error {
        message: String,
        retry: Option<String>,
    },
}

/// Static feedback for one bridge call.
#[derive(Clone, Copy, Debug)]
pub struct OpSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub done: &'static str,
    pub surface: Surface,
    pub retry: bool,
    pub cancel: bool,
    pub indeterminate: bool,
    pub toast_ok: bool,
    pub toast_err: bool,
}

/// A toast the overlay can paint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastView {
    pub op: String,
    pub message: String,
    pub bad: bool,
    pub retry: bool,
    pub details: String,
}

#[derive(Clone, Debug)]
struct Slot {
    state: OpState,
    started_at: u64,
    label: String,
    row: Option<String>,
    finished_at: Option<u64>,
    surface: Surface,
    details: String,
}

#[derive(Clone, Debug)]
struct Toast {
    op: String,
    message: String,
    bad: bool,
    retry: bool,
    until: u64,
    details: String,
    dismissed: bool,
}

#[derive(Clone, Debug)]
struct Latest {
    text: String,
    until: u64,
}

/// Keyed phases. Missing keys are idle.
#[derive(Clone, Debug)]
pub struct Activity {
    slots: BTreeMap<String, Slot>,
    toasts: Vec<Toast>,
    latest: Option<Latest>,
    /// `Some` replaces wall time. Tests advance this.
    frozen: Option<u64>,
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    pub fn new() -> Self {
        let mut slots = BTreeMap::new();
        for spec in operation_catalog() {
            slots.insert(spec.id.to_string(), Slot::idle(spec));
        }
        Self {
            slots,
            toasts: Vec::new(),
            latest: None,
            frozen: None,
        }
    }

    /// Pin the clock. Later [`Self::advance`] calls move it.
    #[cfg(test)]
    pub fn freeze(&mut self, now: u64) {
        self.frozen = Some(now);
    }

    #[cfg(test)]
    pub fn advance(&mut self, ms: u64) {
        let now = self.now().saturating_add(ms);
        self.frozen = Some(now);
    }

    pub fn now(&self) -> u64 {
        self.frozen.unwrap_or_else(wall_ms)
    }

    #[cfg(test)]
    pub fn is_registered(&self, id: &str) -> bool {
        self.slots.contains_key(id)
    }

    pub fn state(&self, id: &str) -> OpState {
        self.slots
            .get(id)
            .map(|slot| slot.state.clone())
            .unwrap_or(OpState::Idle)
    }

    pub fn done_text(&self, id: &str) -> &str {
        spec(id).map(|item| item.done).unwrap_or("Done.")
    }

    pub fn row(&self, id: &str) -> Option<String> {
        self.slots.get(id).and_then(|slot| slot.row.clone())
    }

    pub fn details(&self, id: &str) -> String {
        match self.slots.get(id) {
            Some(slot) if !slot.details.is_empty() => format!("{id}: {}", slot.details),
            Some(slot) if !slot.label.is_empty() => format!("{id}: {}", slot.label),
            _ => id.to_string(),
        }
    }

    pub fn can_retry(&self, id: &str) -> bool {
        spec(id).is_some_and(|item| item.retry)
            && matches!(self.state(id), OpState::Error { retry: Some(_), .. })
    }

    /// Start `id`. A second start replaces the previous phase.
    pub fn begin(&mut self, id: &str, row: Option<String>) {
        let known = spec(id);
        let label = known
            .map(|item| item.label.to_string())
            .unwrap_or_else(|| "Working…".to_string());
        let surface = known.map(|item| item.surface).unwrap_or(Surface::Global);
        let now = self.now();
        let slot = self
            .slots
            .entry(id.to_string())
            .or_insert_with(|| Slot::plain(surface));
        slot.state = OpState::Loading {
            started_at: now,
            label: label.clone(),
        };
        slot.started_at = now;
        slot.label = label;
        slot.row = row;
        slot.finished_at = None;
        slot.surface = surface;
        slot.details.clear();
        self.toasts.retain(|toast| toast.op != id);
    }

    /// Record success. Does nothing when `id` is not loading, so a refresh
    /// that nobody started cannot toast.
    pub fn succeed(&mut self, id: &str, message: impl Into<String>) {
        let message = message.into();
        let toast_ok = spec(id).is_some_and(|item| item.toast_ok);
        let now = self.now();
        {
            let Some(slot) = self.slots.get_mut(id) else {
                return;
            };
            if !matches!(slot.state, OpState::Loading { .. }) {
                return;
            }
            slot.state = OpState::Success {
                message: message.clone(),
            };
            slot.finished_at = Some(now);
            slot.details = message.clone();
        }
        if toast_ok {
            self.latest = Some(Latest {
                text: message.clone(),
                until: now.saturating_add(RESULT_HOLD_MS),
            });
            self.toasts.push(Toast {
                op: id.to_string(),
                message,
                bad: false,
                retry: false,
                until: now.saturating_add(RESULT_HOLD_MS),
                details: String::new(),
                dismissed: false,
            });
        }
    }

    /// Record a failure. Does nothing unless `id` is loading, so a stale
    /// callback cannot toast. Retry is set only when the catalog allows it.
    pub fn fail(&mut self, id: &str, message: impl Into<String>) {
        let message = message.into();
        if !matches!(self.state(id), OpState::Loading { .. }) {
            return;
        }
        let known = spec(id);
        let retry = known.is_some_and(|item| item.retry);
        let now = self.now();
        {
            let Some(slot) = self.slots.get_mut(id) else {
                return;
            };
            slot.state = OpState::Error {
                message: message.clone(),
                retry: retry.then(|| id.to_string()),
            };
            slot.finished_at = Some(now);
            slot.details = message.clone();
        }
        let toast_err = known.is_none_or(|item| item.toast_err);
        if toast_err {
            self.latest = Some(Latest {
                text: message.clone(),
                until: now.saturating_add(RESULT_HOLD_MS),
            });
            self.toasts.retain(|toast| toast.op != id);
            self.toasts.push(Toast {
                op: id.to_string(),
                message,
                bad: true,
                retry,
                until: u64::MAX,
                details: String::new(),
                dismissed: false,
            });
        }
    }

    /// Drop a load that a newer refresh replaced.
    pub fn abandon(&mut self, id: &str) {
        if let Some(slot) = self.slots.get_mut(id)
            && matches!(slot.state, OpState::Loading { .. })
        {
            slot.state = OpState::Idle;
            slot.finished_at = None;
            slot.row = None;
        }
    }

    pub fn spinner_on(&self, id: &str) -> bool {
        self.slots
            .get(id)
            .is_some_and(|slot| self.spinner_slot(slot))
    }

    pub fn is_loading(&self, id: &str) -> bool {
        matches!(self.state(id), OpState::Loading { .. })
    }

    /// Label of a spinning operation whose row is `row`, such as "Pushing…".
    pub fn row_label(&self, row: &str) -> Option<String> {
        self.slots.values().find_map(|slot| {
            if self.spinner_slot(slot) && slot.row.as_deref() == Some(row) {
                Some(slot.label.clone())
            } else {
                None
            }
        })
    }

    pub fn surface_busy(&self, surface: Surface) -> bool {
        self.slots
            .values()
            .any(|slot| self.covers(slot, surface) && self.spinner_slot(slot))
    }

    /// Loading, including the quiet moment before the spinner.
    pub fn surface_pending(&self, surface: Surface) -> bool {
        self.slots
            .values()
            .any(|slot| self.covers(slot, surface) && matches!(slot.state, OpState::Loading { .. }))
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn activity_count(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| self.spinner_slot(slot))
            .count()
    }

    #[cfg(feature = "devtools")]
    pub fn loading_ids(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter(|(_, slot)| matches!(slot.state, OpState::Loading { .. }))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Strip sentence. `None` lets the caller keep the fetch line.
    pub fn strip_line(&self) -> Option<String> {
        let mut spinning: Vec<&Slot> = self
            .slots
            .values()
            .filter(|slot| self.spinner_slot(slot))
            .collect();
        if !spinning.is_empty() {
            spinning.sort_by_key(|slot| slot.started_at);
            let label = spinning
                .last()
                .map(|slot| slot.label.as_str())
                .unwrap_or("Working…");
            return Some(format!("{} in progress: {label}", spinning.len()));
        }
        let now = self.now();
        self.latest
            .as_ref()
            .filter(|latest| now < latest.until)
            .map(|latest| latest.text.clone())
    }

    pub fn visible_toasts(&self) -> Vec<ToastView> {
        let now = self.now();
        self.toasts
            .iter()
            .filter(|toast| self.toast_visible(toast, now))
            .map(|toast| ToastView {
                op: toast.op.clone(),
                message: toast.message.clone(),
                bad: toast.bad,
                retry: toast.retry,
                details: toast.details.clone(),
            })
            .collect()
    }

    pub fn dismiss_toasts(&mut self) {
        for toast in &mut self.toasts {
            toast.dismissed = true;
        }
    }

    #[cfg(test)]
    pub fn dismiss_op(&mut self, id: &str) {
        for toast in &mut self.toasts {
            if toast.op == id {
                toast.dismissed = true;
            }
        }
    }

    /// A success toast that already expired should not fall back to the old line.
    pub fn hides_legacy(&self, text: Option<&str>) -> bool {
        let Some(text) = text else {
            return false;
        };
        let now = self.now();
        self.toasts.iter().any(|toast| {
            !toast.bad && toast.message == text && (toast.dismissed || now >= toast.until)
        })
    }

    pub fn loading_mutation(&self) -> Option<String> {
        const IDS: &[&str] = &[
            "assign",
            "discard",
            "create_lane",
            "commit_create",
            "amend",
            "reword",
            "squash",
            "uncommit",
            "uncommit_changes",
            "move_commit",
            "insert_blank",
            "move_changes",
            "undo_last",
        ];
        IDS.iter()
            .copied()
            .find(|id| self.is_loading(id))
            .map(str::to_string)
    }

    fn toast_visible(&self, toast: &Toast, now: u64) -> bool {
        if toast.dismissed || now >= toast.until {
            return false;
        }
        if toast.bad {
            return true;
        }
        self.slots
            .get(&toast.op)
            .is_none_or(|slot| !self.spinner_slot(slot))
    }

    fn spinner_slot(&self, slot: &Slot) -> bool {
        if matches!(slot.state, OpState::Error { .. } | OpState::Idle) {
            return false;
        }
        let now = self.now();
        let reveal = slot.started_at.saturating_add(SLOW_START_MS);
        match slot.state {
            OpState::Loading { .. } => now >= reveal,
            OpState::Success { .. } => {
                let Some(finished) = slot.finished_at else {
                    return false;
                };
                if finished < reveal {
                    return false;
                }
                now < reveal.saturating_add(MIN_VISIBLE_MS)
            }
            OpState::Error { .. } | OpState::Idle => false,
        }
    }

    fn covers(&self, slot: &Slot, surface: Surface) -> bool {
        if slot.surface == surface {
            return true;
        }
        slot.surface == Surface::Workspace
            && matches!(
                surface,
                Surface::Uncommitted | Surface::Diff | Surface::BranchesColumn
            )
    }
}

impl Slot {
    fn idle(spec: &OpSpec) -> Self {
        Self {
            state: OpState::Idle,
            started_at: 0,
            label: spec.label.to_string(),
            row: None,
            finished_at: None,
            surface: spec.surface,
            details: String::new(),
        }
    }

    fn plain(surface: Surface) -> Self {
        Self {
            state: OpState::Idle,
            started_at: 0,
            label: String::new(),
            row: None,
            finished_at: None,
            surface,
            details: String::new(),
        }
    }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub fn spec(id: &str) -> Option<&'static OpSpec> {
    operation_catalog().iter().find(|item| item.id == id)
}

/// Every user-facing bridge call, plus the refresh that bundles several reads.
pub fn operation_catalog() -> &'static [OpSpec] {
    use Surface::*;
    #[allow(clippy::too_many_arguments)]
    const fn op(
        id: &'static str,
        label: &'static str,
        done: &'static str,
        surface: Surface,
        retry: bool,
        cancel: bool,
        toast_ok: bool,
        toast_err: bool,
    ) -> OpSpec {
        OpSpec {
            id,
            label,
            done,
            surface,
            retry,
            cancel,
            indeterminate: true,
            toast_ok,
            toast_err,
        }
    }
    const CATALOG: &[OpSpec] = &[
        op(
            "list_projects",
            "Loading projects…",
            "Projects are up to date.",
            Workspace,
            true,
            false,
            false,
            true,
        ),
        op(
            "open_project",
            "Opening the project…",
            "The project is open.",
            Workspace,
            true,
            false,
            false,
            true,
        ),
        op(
            "worktree",
            "Reading changes…",
            "Changes are up to date.",
            Uncommitted,
            true,
            false,
            false,
            true,
        ),
        op(
            "refresh_status",
            "Refreshing…",
            "Refresh finished.",
            Workspace,
            true,
            false,
            false,
            true,
        ),
        op(
            "file_diff",
            "Reading the diff…",
            "The diff is ready.",
            Diff,
            true,
            false,
            false,
            true,
        ),
        op(
            "commit_details",
            "Reading the commit…",
            "The commit is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "branch_files",
            "Reading the branch…",
            "The branch files are ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "assign",
            "Moving changes…",
            "Moved the changes.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "discard",
            "Discarding…",
            "Discarded the changes.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "create_lane",
            "Creating a lane…",
            "Created the lane.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "commit_create",
            "Committing…",
            "Committed.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "amend",
            "Amending…",
            "Amended the commit.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "reword",
            "Rewording…",
            "Reworded the commit.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "squash",
            "Squashing…",
            "Squashed the commit.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "uncommit",
            "Uncommitting…",
            "Uncommitted.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "uncommit_changes",
            "Uncommitting changes…",
            "Uncommitted the changes.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "move_commit",
            "Moving the commit…",
            "Moved the commit.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "insert_blank",
            "Inserting a blank commit…",
            "Inserted a blank commit.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "move_changes",
            "Moving changes…",
            "Moved the changes.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "undo_last",
            "Undoing…",
            "Undid the last change.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "snapshots",
            "Loading history…",
            "History is up to date.",
            HistoryPage,
            true,
            false,
            false,
            true,
        ),
        op(
            "list_snapshots",
            "Loading history…",
            "History is up to date.",
            HistoryPage,
            true,
            false,
            false,
            true,
        ),
        op(
            "restore_snapshot",
            "Restoring…",
            "Restored the snapshot.",
            HistoryPage,
            true,
            false,
            true,
            true,
        ),
        op(
            "snapshot_diff",
            "Reading the snapshot…",
            "The snapshot is ready.",
            HistoryPage,
            true,
            false,
            false,
            true,
        ),
        op(
            "create_snapshot",
            "Creating a snapshot…",
            "Created a snapshot.",
            HistoryPage,
            true,
            false,
            true,
            true,
        ),
        op(
            "list_branches",
            "Loading branches…",
            "Branches are up to date.",
            BranchesPage,
            true,
            false,
            false,
            true,
        ),
        op(
            "branch_details",
            "Reading the branch…",
            "The branch is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "apply_branch",
            "Applying…",
            "Applied the branch.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "unapply_branch",
            "Unapplying…",
            "Unapplied the branch.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "delete_local_branch",
            "Deleting the branch…",
            "Deleted the branch.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "edit_status",
            "Checking the edit…",
            "Edit status is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "enter_edit_mode",
            "Opening the commit…",
            "The commit is open for editing.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "save_edit_and_return_to_workspace",
            "Saving the edit…",
            "Saved the edit.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "abort_edit_and_return_to_workspace",
            "Aborting the edit…",
            "Aborted the edit.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "conflicted_commits",
            "Looking for conflicts…",
            "Conflict list is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "resolve_commit_conflict_hunks",
            "Resolving conflicts…",
            "Resolved the conflicts.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "add_project",
            "Adding the project…",
            "Added the project.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "delete_project",
            "Removing the project…",
            "Removed the project.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "forget_project",
            "Removing the project…",
            "Removed the project from this app.",
            Global,
            false,
            false,
            true,
            true,
        ),
        op(
            "set_target",
            "Setting the upstream…",
            "The upstream is set.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "fetch",
            "Fetching…",
            "Fetched.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "fetch_status",
            "Checking upstream…",
            "Upstream status is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "integrate_preview",
            "Checking upstream commits…",
            "Upstream check is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "integrate",
            "Integrating…",
            "Upstream commits are now part of your lanes.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "push",
            "Pushing…",
            "Pushed.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "askpass_pending",
            "Waiting for a sign-in prompt…",
            "The sign-in prompt closed.",
            Global,
            false,
            true,
            false,
            true,
        ),
        op(
            "answer_askpass",
            "Sending the sign-in…",
            "Sign-in sent.",
            Global,
            false,
            true,
            false,
            true,
        ),
        op(
            "publish_draft",
            "Publishing…",
            "Opened the draft.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "suggest_message",
            "Suggesting…",
            "Suggestion is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "read_global",
            "Loading settings…",
            "Settings are loaded.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "update_global",
            "Saving settings…",
            "Saved settings.",
            Global,
            true,
            false,
            true,
            true,
        ),
        op(
            "read_project",
            "Loading project settings…",
            "Project settings are loaded.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "forge_status",
            "Checking pull requests…",
            "Pull requests are up to date.",
            Global,
            true,
            false,
            false,
            false,
        ),
        op(
            "review_for_branch",
            "Checking the pull request…",
            "The pull request is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "remote_branches",
            "Loading remote branches…",
            "Remote branches are loaded.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "base_branch",
            "Reading the target branch…",
            "The target branch is ready.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "pre_commit_hook",
            "Running the commit hook…",
            "Commit hook finished.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "post_commit_hook",
            "Finishing the commit hook…",
            "Commit hook finished.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "message_hook",
            "Checking the message…",
            "Message check finished.",
            Global,
            true,
            false,
            false,
            true,
        ),
        op(
            "watch",
            "Watching the project…",
            "Watching the project.",
            Global,
            true,
            false,
            false,
            false,
        ),
    ];
    CATALOG
}

/// Bridge calls that must have a feedback slot. A new method is added here and
/// in [`operation_catalog`], or the coverage test fails.
#[cfg(test)]
pub const BRIDGE_OPS: &[&str] = &[
    "list_projects",
    "open_project",
    "worktree",
    "refresh_status",
    "file_diff",
    "commit_details",
    "branch_files",
    "assign",
    "discard",
    "create_lane",
    "commit_create",
    "amend",
    "reword",
    "squash",
    "uncommit",
    "uncommit_changes",
    "move_commit",
    "insert_blank",
    "move_changes",
    "undo_last",
    "snapshots",
    "list_snapshots",
    "restore_snapshot",
    "snapshot_diff",
    "create_snapshot",
    "list_branches",
    "branch_details",
    "apply_branch",
    "unapply_branch",
    "delete_local_branch",
    "edit_status",
    "enter_edit_mode",
    "save_edit_and_return_to_workspace",
    "abort_edit_and_return_to_workspace",
    "conflicted_commits",
    "resolve_commit_conflict_hunks",
    "add_project",
    "delete_project",
    "forget_project",
    "set_target",
    "fetch",
    "fetch_status",
    "integrate_preview",
    "integrate",
    "push",
    "askpass_pending",
    "answer_askpass",
    "publish_draft",
    "suggest_message",
    "read_global",
    "update_global",
    "read_project",
    "forge_status",
    "review_for_branch",
    "remote_branches",
    "base_branch",
    "pre_commit_hook",
    "post_commit_hook",
    "message_hook",
    "watch",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bridge_operation_has_feedback() {
        let catalog: Vec<&str> = operation_catalog().iter().map(|item| item.id).collect();
        for id in BRIDGE_OPS {
            let item = spec(id).unwrap_or_else(|| panic!("{id} has no feedback state"));
            assert!(!item.label.is_empty(), "{id} label");
            assert!(!item.done.is_empty(), "{id} done");
            assert!(
                item.label.ends_with('…'),
                "{id} label should be a gerund, got {}",
                item.label
            );
            let activity = Activity::new();
            assert!(
                activity.is_registered(id),
                "{id} is missing from the registry"
            );
            assert!(
                matches!(activity.state(id), OpState::Idle),
                "{id} should start idle"
            );
        }
        for id in &catalog {
            assert!(
                BRIDGE_OPS.contains(id),
                "{id} is registered but not in the bridge list"
            );
        }
        assert_eq!(catalog.len(), BRIDGE_OPS.len());
        let push = spec("push").unwrap();
        let fetch = spec("fetch").unwrap();
        assert!(push.indeterminate && fetch.indeterminate);
        assert!(!push.cancel && !fetch.cancel);
        assert!(push.retry && fetch.retry);
        assert!(spec("answer_askpass").unwrap().cancel);
        assert!(spec("askpass_pending").unwrap().cancel);
        assert!(!spec("discard").unwrap().retry);
        assert!(!spec("delete_local_branch").unwrap().retry);
    }

    #[test]
    fn phases_move_and_the_spinner_waits() {
        let mut activity = Activity::new();
        activity.freeze(1_000);
        assert!(matches!(activity.state("push"), OpState::Idle));
        activity.begin("push", Some("lane-c".into()));
        assert!(matches!(
            activity.state("push"),
            OpState::Loading {
                started_at: 1_000,
                ..
            }
        ));
        assert!(!activity.spinner_on("push"));
        assert_eq!(activity.activity_count(), 0);
        activity.advance(SLOW_START_MS - 1);
        assert!(!activity.spinner_on("push"));
        activity.advance(1);
        assert!(activity.spinner_on("push"));
        assert_eq!(activity.row_label("lane-c").as_deref(), Some("Pushing…"));
        assert_eq!(activity.activity_count(), 1);
        assert!(
            activity
                .strip_line()
                .unwrap()
                .contains("1 in progress: Pushing…")
        );
        activity.succeed("push", "Pushed 2 commits.");
        assert!(matches!(activity.state("push"), OpState::Success { .. }));
        assert!(activity.spinner_on("push"), "minimum visible time");
        assert!(
            activity.visible_toasts().is_empty(),
            "toast waits out the hold"
        );
        activity.advance(MIN_VISIBLE_MS - 1);
        assert!(activity.spinner_on("push"));
        activity.advance(1);
        assert!(!activity.spinner_on("push"));
        let toasts = activity.visible_toasts();
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].message, "Pushed 2 commits.");
        assert!(!toasts[0].bad);
        activity.advance(RESULT_HOLD_MS);
        assert!(activity.visible_toasts().is_empty());
    }

    #[test]
    fn a_fast_success_never_shows_a_spinner() {
        let mut activity = Activity::new();
        activity.freeze(0);
        activity.begin("fetch", None);
        activity.advance(40);
        activity.succeed("fetch", "Up to date.");
        activity.advance(5_000);
        assert!(!activity.spinner_on("fetch"));
        assert!(activity.visible_toasts().is_empty());
        activity.freeze(0);
        activity.begin("fetch", None);
        activity.succeed("fetch", "Up to date.");
        let toasts = activity.visible_toasts();
        assert_eq!(toasts.len(), 1);
        assert!(!activity.spinner_on("fetch"));
        activity.advance(RESULT_HOLD_MS);
        assert!(activity.visible_toasts().is_empty());
        assert!(activity.hides_legacy(Some("Up to date.")));
    }

    #[test]
    fn errors_keep_retry_and_do_not_wait_for_the_spinner() {
        let mut activity = Activity::new();
        activity.freeze(50);
        activity.begin("fetch", None);
        activity.fail("fetch", "Could not reach the remote. You might be offline.");
        assert!(!activity.spinner_on("fetch"));
        match activity.state("fetch") {
            OpState::Error {
                retry: Some(op),
                message,
            } => {
                assert_eq!(op, "fetch");
                assert!(message.contains("offline"));
            }
            other => panic!("expected error, got {other:?}"),
        }
        assert!(activity.can_retry("fetch"));
        let toast = &activity.visible_toasts()[0];
        assert!(toast.bad && toast.retry);
        assert!(activity.details("fetch").contains("offline"));
        activity.begin("discard", None);
        activity.fail("discard", "The discard failed.");
        assert!(!activity.can_retry("discard"));
        assert!(
            !activity
                .visible_toasts()
                .iter()
                .any(|toast| toast.op == "discard" && toast.retry)
        );
        activity.dismiss_op("fetch");
        assert!(
            activity
                .visible_toasts()
                .iter()
                .all(|toast| toast.op != "fetch")
        );
    }

    #[test]
    fn succeed_ignores_an_operation_that_never_started() {
        let mut activity = Activity::new();
        activity.freeze(0);
        activity.succeed("refresh_status", "Refresh finished.");
        assert!(matches!(activity.state("refresh_status"), OpState::Idle));
        assert!(activity.strip_line().is_none());
        assert!(activity.visible_toasts().is_empty());
    }

    #[test]
    fn workspace_refresh_covers_every_column_after_the_slow_start() {
        let mut activity = Activity::new();
        activity.freeze(0);
        activity.begin("refresh_status", None);
        activity.begin("list_projects", None);
        activity.begin("worktree", None);
        assert!(activity.surface_pending(Surface::Uncommitted));
        assert!(!activity.surface_busy(Surface::Diff));
        activity.advance(SLOW_START_MS);
        assert!(activity.surface_busy(Surface::Uncommitted));
        assert!(activity.surface_busy(Surface::Diff));
        assert!(activity.surface_busy(Surface::BranchesColumn));
        assert!(!activity.surface_busy(Surface::HistoryPage));
        activity.abandon("refresh_status");
        activity.abandon("list_projects");
        activity.abandon("worktree");
        assert!(!activity.surface_pending(Surface::Uncommitted));
    }
}
