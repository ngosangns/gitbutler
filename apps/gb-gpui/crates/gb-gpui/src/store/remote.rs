//! Fetch, integrate, push, project setup, and the askpass dialog.
//!
//! No GPUI types. The window runs a [`RemoteOp`] through [`perform_remote`],
//! then [`Store::accept_remote`]. The password buffer is cleared on close and
//! is never included in [`Debug`] or in the dev-socket flags.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use gb_bridge::{
    AskpassKind, AskpassRequest, Backend, FetchStatus, ForcePush, IntegratePreview,
    IntegrateReport, IntegrateResolution, ProjectSummary, PushReport, RemoteBranch, UpstreamCommit,
    UpstreamStrategy,
};

use super::{Effect, Store};

pub(crate) fn remote_activity(op: &RemoteOp) -> (&'static str, Option<String>) {
    match op {
        RemoteOp::Fetch => ("fetch", None),
        RemoteOp::LoadBranches => ("remote_branches", None),
        RemoteOp::SetTarget { target_ref, .. } => ("set_target", Some(target_ref.clone())),
        RemoteOp::AddProject { path } => ("add_project", Some(path.clone())),
        RemoteOp::Preview { .. } => ("integrate_preview", None),
        RemoteOp::Apply { .. } => ("integrate", None),
        RemoteOp::Push { lane, .. } => ("push", Some(lane.clone())),
        RemoteOp::Answer { cancel: true, .. } => ("askpass_pending", None),
        RemoteOp::Answer { cancel: false, .. } => ("answer_askpass", None),
    }
}

fn remote_settle(done: &RemoteDone) -> (&'static str, String, bool) {
    match done {
        RemoteDone::Fetched(status) => {
            let summary = if status.summary.is_empty() {
                "Fetched.".to_string()
            } else {
                status.summary.clone()
            };
            ("fetch", summary, false)
        }
        RemoteDone::Branches(_) => (
            "remote_branches",
            "Remote branches are loaded.".to_string(),
            false,
        ),
        RemoteDone::Target { .. } => ("set_target", "The upstream is set.".to_string(), false),
        RemoteDone::Added(_) => ("add_project", "Added the project.".to_string(), false),
        RemoteDone::Preview(preview) => {
            let bad = !preview.worktree_conflicts.is_empty();
            let message = if bad {
                format!(
                    "Uncommitted changes conflict: {}",
                    preview.worktree_conflicts.join(", ")
                )
            } else {
                "Upstream check is ready.".to_string()
            };
            ("integrate_preview", message, bad)
        }
        RemoteDone::Applied(report) => {
            let bad = !report.worktree_conflicts.is_empty();
            let message = if bad {
                format!(
                    "Some uncommitted changes conflicted: {}",
                    report.worktree_conflicts.join(", ")
                )
            } else {
                "Upstream commits are now part of your lanes.".to_string()
            };
            ("integrate", message, bad)
        }
        RemoteDone::Pushed { text, .. } => {
            let lower = text.to_ascii_lowercase();
            let bad = lower.contains("rejected") || lower.contains("did not");
            ("push", text.clone(), bad)
        }
        RemoteDone::Answered => ("answer_askpass", "Sign-in sent.".to_string(), false),
    }
}

/// One remote call. The askpass text is not stored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOp {
    Fetch,
    LoadBranches,
    SetTarget {
        target_ref: String,
        push_remote: Option<String>,
    },
    AddProject {
        path: String,
    },
    Preview {
        strategy: UpstreamStrategy,
    },
    Apply {
        strategy: UpstreamStrategy,
    },
    Push {
        lane: String,
        force: ForcePush,
        /// Commits on the lane that were not on the remote before this push.
        unpushed: usize,
    },
    Answer {
        id: String,
        cancel: bool,
    },
}

/// What the bridge returned. No credential text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDone {
    Fetched(FetchStatus),
    Branches(Vec<RemoteBranch>),
    Target { target_ref: String },
    Added(ProjectSummary),
    Preview(IntegratePreview),
    Applied(IntegrateReport),
    Pushed { lane: String, text: String },
    Answered,
}

/// A force push waiting for the destructive confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushConfirm {
    pub lane: String,
    pub force: ForcePush,
    pub title: String,
    pub message: String,
}

/// Setup, fetch, integrate, and push. The askpass draft is private.
#[derive(Clone, Default)]
pub struct RemoteUi {
    pub branches: Vec<RemoteBranch>,
    pub branches_loaded: bool,
    pub branch_index: usize,
    pub push_remote: String,
    pub push_remote_edited: bool,
    pub push_remote_focused: bool,
    pub setup_busy: bool,
    pub setup_error: Option<String>,
    pub add_open: bool,
    pub add_path: String,
    pub add_focused: bool,
    pub fetching: bool,
    pub fetch_summary: Option<String>,
    pub upstream_count: usize,
    pub upstream: Vec<UpstreamCommit>,
    pub last_fetched_ms: Option<u128>,
    pub fetch_error: Option<String>,
    pub integrate_open: bool,
    pub integrate_cursor: usize,
    pub strategies: BTreeMap<String, UpstreamStrategy>,
    pub preview: Option<IntegratePreview>,
    pub preview_busy: bool,
    pub integrate_busy: BTreeSet<String>,
    pub integrate_error: Option<String>,
    pub integrate_confirm: bool,
    pub push_menu: Option<String>,
    pub push_cursor: usize,
    pub push_confirm: Option<PushConfirm>,
    pub push_busy: Option<String>,
    pub push_results: BTreeMap<String, String>,
    pub askpass: Option<AskpassRequest>,
    draft: String,
    pub reveal: bool,
    pub submitting: bool,
}

impl fmt::Debug for RemoteUi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteUi")
            .field("branches", &self.branches.len())
            .field("branch_index", &self.branch_index)
            .field("push_remote", &self.push_remote)
            .field("setup_busy", &self.setup_busy)
            .field("setup_error", &self.setup_error)
            .field("add_open", &self.add_open)
            .field("fetching", &self.fetching)
            .field("fetch_summary", &self.fetch_summary)
            .field("upstream_count", &self.upstream_count)
            .field("integrate_open", &self.integrate_open)
            .field("preview_busy", &self.preview_busy)
            .field("integrate_busy", &self.integrate_busy)
            .field("integrate_error", &self.integrate_error)
            .field("integrate_confirm", &self.integrate_confirm)
            .field("push_menu", &self.push_menu)
            .field(
                "push_confirm",
                &self.push_confirm.as_ref().map(|ask| &ask.lane),
            )
            .field("push_busy", &self.push_busy)
            .field("push_results", &self.push_results)
            .field("askpass_open", &self.askpass.is_some())
            .field(
                "askpass_kind",
                &self.askpass.as_ref().map(|request| request.kind),
            )
            .field(
                "askpass_draft",
                &if self.draft.is_empty() {
                    "empty"
                } else {
                    "[redacted]"
                },
            )
            .finish()
    }
}

impl RemoteUi {
    pub fn on_project_change(&mut self) {
        let askpass = self.askpass.clone();
        let draft = std::mem::take(&mut self.draft);
        let reveal = self.reveal;
        let submitting = self.submitting;
        *self = Self::default();
        self.askpass = askpass;
        self.draft = draft;
        self.reveal = reveal;
        self.submitting = submitting;
    }

    pub fn close_overlays(&mut self) {
        self.integrate_open = false;
        self.integrate_confirm = false;
        self.preview_busy = false;
        self.push_menu = None;
        self.push_confirm = None;
    }

    /// Fixed status text. "Not fetched yet" until a fetch records a time.
    /// A fetch in progress keeps the previous summary. The button says
    /// "Fetching…" once the slow-start has passed.
    pub fn status_line(&self) -> String {
        if let Some(error) = &self.fetch_error {
            return error.clone();
        }
        if self.last_fetched_ms.is_none() {
            return "Not fetched yet".to_string();
        }
        self.fetch_summary
            .clone()
            .unwrap_or_else(|| "Not fetched yet".to_string())
    }

    pub fn fetch_tip(&self) -> String {
        let when = match self.last_fetched_ms {
            Some(ms) => format!("Last fetched {}. ", format_fetched(ms)),
            None => "Not fetched yet. ".to_string(),
        };
        format!("{when}Fetch downloads new commits from the remote. It does not change your lanes.")
    }

    pub fn upstream_chip(&self) -> Option<String> {
        match self.upstream_count {
            0 => None,
            1 => Some("1 new upstream commit".to_string()),
            n => Some(format!("{n} new upstream commits")),
        }
    }

    pub fn set_branches(&mut self, branches: Vec<RemoteBranch>) {
        self.branches_loaded = true;
        self.branches = branches;
        if self.branch_index >= self.branches.len() {
            self.branch_index = 0;
        }
        if !self.push_remote_edited
            && let Some(branch) = self.branches.get(self.branch_index)
        {
            self.push_remote = branch.remote.clone();
        }
    }

    pub fn move_branch(&mut self, delta: i32) {
        if self.branches.is_empty() {
            return;
        }
        let next = self.branch_index as i32 + delta;
        let last = self.branches.len() as i32 - 1;
        self.branch_index = next.clamp(0, last) as usize;
        if !self.push_remote_edited
            && let Some(branch) = self.branches.get(self.branch_index)
        {
            self.push_remote = branch.remote.clone();
        }
    }

    pub fn selected_branch(&self) -> Option<&RemoteBranch> {
        self.branches.get(self.branch_index)
    }

    pub fn note_fetch(&mut self, status: FetchStatus) {
        self.fetching = false;
        self.fetch_error = None;
        self.upstream_count = status.upstream_count;
        self.upstream = status.commits;
        self.last_fetched_ms = status.last_fetched_ms;
        self.fetch_summary = Some(status.summary);
        if status.upstream_count == 0 {
            self.integrate_open = false;
            self.integrate_confirm = false;
            self.preview_busy = false;
        }
    }

    pub fn strategy_of(&self, lane: &str) -> UpstreamStrategy {
        self.strategies
            .get(lane)
            .copied()
            .unwrap_or(UpstreamStrategy::Rebase)
    }

    pub fn lane_outcome(&self, lane: &str) -> &'static str {
        let Some(preview) = &self.preview else {
            return "Not checked yet";
        };
        match preview
            .resolutions
            .iter()
            .find(|lane_row| lane_row.lane == lane)
            .map(|lane_row| lane_row.resolution)
        {
            Some(IntegrateResolution::Clear) => "Clean",
            Some(IntegrateResolution::Conflicted) => "Conflicts",
            Some(IntegrateResolution::Integrated) | None => "Already integrated",
        }
    }

    /// One strategy for every lane. The bridge cannot mix Rebase and Merge.
    pub fn planned_strategy(&self, lanes: &[String]) -> Result<UpstreamStrategy, String> {
        if lanes.is_empty() {
            return Err("This project has no lanes to update.".to_string());
        }
        let first = self.strategy_of(&lanes[0]);
        if lanes.iter().any(|lane| self.strategy_of(lane) != first) {
            return Err(
                "GitButler updates every lane the same way. Choose Rebase for all of them, or Merge for all of them."
                    .to_string(),
            );
        }
        Ok(first)
    }

    pub fn shown_secret(&self) -> String {
        mask_secret(&self.draft, self.reveal)
    }

    pub fn take_draft(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.draft);
        self.reveal = false;
        if text.is_empty() { None } else { Some(text) }
    }

    pub fn clear_secret(&mut self) {
        self.draft.clear();
        self.reveal = false;
        self.submitting = false;
    }
}

impl Store {
    pub fn needs_setup(&self) -> bool {
        !self.blocks_open()
            && self
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.target_branch.is_none())
    }

    /// One line of flags for tests. The askpass draft is not included.
    #[cfg(test)]
    pub fn remote_state_line(&self) -> String {
        format!(
            "needs_setup={} fetching={} fetch_summary={} upstream={} integrate_open={} push_menu={} push_confirm={} askpass_open={}",
            self.needs_setup(),
            self.remote.fetching,
            self.remote.status_line(),
            self.remote.upstream_count,
            self.remote.integrate_open,
            self.remote.push_menu.as_deref().unwrap_or(""),
            self.remote.push_confirm.is_some(),
            self.remote.askpass.is_some(),
        )
    }

    pub fn request_fetch(&mut self) -> Effect {
        if self.selected.is_none() {
            self.toast_bad = true;
            self.toast = Some("Choose a project first.".to_string());
            return Effect::None;
        }
        if self.remote.fetching {
            return Effect::None;
        }
        self.remote.fetching = true;
        self.remote.fetch_error = None;
        Effect::Remote(RemoteOp::Fetch)
    }

    pub fn request_branches(&mut self) -> Effect {
        if self.selected.is_none() || self.remote.setup_busy || self.remote.branches_loaded {
            return Effect::None;
        }
        self.remote.setup_busy = true;
        self.remote.setup_error = None;
        Effect::Remote(RemoteOp::LoadBranches)
    }

    pub fn request_add(&mut self, path: String) -> Effect {
        let path = path.trim().to_string();
        if path.is_empty() {
            self.remote.setup_error = Some("Type the folder path of the repository.".to_string());
            return Effect::None;
        }
        if self.blocks_open() {
            self.toast_bad = true;
            self.toast = Some(
                "Projects stay closed until the installed GitButler matches this build."
                    .to_string(),
            );
            return Effect::None;
        }
        self.remote.add_path = path.clone();
        self.remote.setup_error = None;
        Effect::Remote(RemoteOp::AddProject { path })
    }

    pub fn request_set_target(
        &mut self,
        target_ref: String,
        push_remote: Option<String>,
    ) -> Result<Effect, String> {
        if self.selected.is_none() {
            return Err("Choose a project first.".to_string());
        }
        let target_ref = target_ref.trim().to_string();
        if target_ref.is_empty() {
            return Err("Choose a target branch.".to_string());
        }
        let push_remote = push_remote
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .or_else(|| {
                let name = self.remote.push_remote.trim();
                if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                }
            });
        self.remote.setup_busy = true;
        self.remote.setup_error = None;
        Ok(Effect::Remote(RemoteOp::SetTarget {
            target_ref,
            push_remote,
        }))
    }

    pub fn request_selected_target(&mut self) -> Effect {
        let Some(branch) = self.remote.selected_branch().cloned() else {
            self.remote.setup_error = Some("Choose a target branch.".to_string());
            return Effect::None;
        };
        match self.request_set_target(branch.full_name, None) {
            Ok(effect) => effect,
            Err(error) => {
                self.remote.setup_error = Some(error);
                Effect::None
            }
        }
    }

    pub fn open_add_field(&mut self) {
        self.remote.add_open = true;
        self.remote.add_focused = true;
        self.project_menu = true;
    }

    pub fn open_integrate(&mut self) -> Effect {
        if self.mutation_blocked().is_some() {
            self.note_edit_lock();
            return Effect::None;
        }
        if self.remote.upstream_count == 0 {
            self.toast_bad = true;
            self.toast = Some("No new upstream commits.".to_string());
            return Effect::None;
        }
        self.remote.push_menu = None;
        self.remote.push_confirm = None;
        self.confirm = None;
        let lanes = self.lane_names();
        for lane in &lanes {
            self.remote
                .strategies
                .entry(lane.clone())
                .or_insert(UpstreamStrategy::Rebase);
        }
        self.remote.integrate_open = true;
        self.remote.integrate_cursor = 0;
        self.remote.integrate_confirm = false;
        self.remote.integrate_error = None;
        match self.remote.planned_strategy(&lanes) {
            Ok(strategy) => {
                self.remote.preview_busy = true;
                Effect::Remote(RemoteOp::Preview { strategy })
            }
            Err(error) => {
                self.remote.integrate_error = Some(error);
                Effect::None
            }
        }
    }

    pub fn confirm_integrate(&mut self) -> Effect {
        if self.mutation_blocked().is_some() {
            self.note_edit_lock();
            return Effect::None;
        }
        if !self.remote.integrate_open && self.remote.upstream_count == 0 {
            self.toast_bad = true;
            self.toast = Some("No new upstream commits.".to_string());
            return Effect::None;
        }
        let lanes = self.lane_names();
        let strategy = match self.remote.planned_strategy(&lanes) {
            Ok(strategy) => strategy,
            Err(error) => {
                self.remote.integrate_error = Some(error);
                self.remote.integrate_confirm = false;
                return Effect::None;
            }
        };
        self.remote.integrate_confirm = false;
        self.remote.integrate_error = None;
        self.remote.integrate_busy = lanes.into_iter().collect();
        self.remote.preview_busy = false;
        Effect::Remote(RemoteOp::Apply { strategy })
    }

    pub fn ask_integrate(&mut self) {
        let lanes = self.lane_names();
        match self.remote.planned_strategy(&lanes) {
            Ok(strategy) => {
                self.remote.integrate_error = None;
                self.remote.integrate_confirm = true;
                self.remote.push_confirm = None;
                self.confirm = None;
                let _ = strategy;
            }
            Err(error) => {
                self.remote.integrate_error = Some(error);
                self.remote.integrate_confirm = false;
            }
        }
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn open_push_menu(&mut self, lane: &str) -> Result<(), String> {
        if let Some(reason) = self.mutation_blocked() {
            self.note_edit_lock();
            return Err(reason.to_string());
        }
        self.ensure_lane(lane)?;
        self.remote.integrate_confirm = false;
        self.remote.push_confirm = None;
        self.confirm = None;
        self.menu = None;
        self.remote.push_menu = Some(lane.to_string());
        self.remote.push_cursor = 0;
        Ok(())
    }

    pub fn push_now(&mut self, lane: &str, force: ForcePush) -> Result<Effect, String> {
        if let Some(reason) = self.mutation_blocked() {
            self.note_edit_lock();
            return Err(reason.to_string());
        }
        self.ensure_lane(lane)?;
        if self.remote.push_busy.is_some() {
            return Err("Wait for the current push to finish.".to_string());
        }
        let unpushed = self.unpushed_count(lane);
        self.remote.push_menu = None;
        self.remote.push_confirm = None;
        self.remote.push_busy = Some(lane.to_string());
        Ok(Effect::Remote(RemoteOp::Push {
            lane: lane.to_string(),
            force,
            unpushed,
        }))
    }

    pub fn ask_force_push(&mut self, lane: &str, force: ForcePush) -> Result<(), String> {
        if let Some(reason) = self.mutation_blocked() {
            self.note_edit_lock();
            return Err(reason.to_string());
        }
        self.ensure_lane(lane)?;
        let (title, message) = match force {
            ForcePush::No => return self.push_now(lane, force).map(|_| ()),
            ForcePush::WithLease => (
                format!("Force push {lane} with lease?"),
                "This replaces the remote branch only if it still matches what you last fetched. Someone else's new commits stop the push.".to_string(),
            ),
            ForcePush::Force => (
                format!("Force push {lane}?"),
                "This overwrites the remote branch, including commits that are not on this lane.".to_string(),
            ),
        };
        self.remote.push_menu = None;
        self.remote.integrate_confirm = false;
        self.confirm = None;
        self.remote.push_confirm = Some(PushConfirm {
            lane: lane.to_string(),
            force,
            title,
            message,
        });
        Ok(())
    }

    pub fn open_askpass(&mut self, request: AskpassRequest) {
        if self
            .remote
            .askpass
            .as_ref()
            .is_some_and(|current| current.id == request.id)
        {
            return;
        }
        self.remote.clear_secret();
        self.remote.askpass = Some(request);
        self.remote.reveal = false;
        self.remote.submitting = false;
    }

    pub fn submit_askpass(&mut self) -> Effect {
        let Some(request) = self.remote.askpass.clone() else {
            return Effect::None;
        };
        self.remote.submitting = true;
        Effect::Remote(RemoteOp::Answer {
            id: request.id,
            cancel: false,
        })
    }

    pub fn cancel_askpass(&mut self) -> Effect {
        let Some(request) = self.remote.askpass.clone() else {
            self.remote.clear_secret();
            return Effect::None;
        };
        self.remote.clear_secret();
        self.remote.askpass = None;
        Effect::Remote(RemoteOp::Answer {
            id: request.id,
            cancel: true,
        })
    }

    pub fn take_askpass_secret(&mut self) -> Option<String> {
        let secret = self.remote.take_draft();
        self.remote.askpass = None;
        self.remote.submitting = false;
        secret
    }

    pub fn abandon_askpass(&mut self) {
        self.remote.askpass = None;
        self.remote.clear_secret();
    }

    pub fn expire_askpass(&mut self) {
        if self.remote.askpass.is_none() {
            return;
        }
        self.remote.askpass = None;
        self.remote.clear_secret();
        self.toast_bad = true;
        self.toast = Some("The sign-in prompt expired.".to_string());
    }

    pub fn note_askpass_pending(&mut self, pending: &[AskpassRequest]) {
        let Some(current) = &self.remote.askpass else {
            return;
        };
        if self.remote.submitting {
            return;
        }
        if !pending.iter().any(|request| request.id == current.id) {
            self.expire_askpass();
        }
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn set_askpass_text(&mut self, text: &str) {
        if self.remote.askpass.is_none() {
            return;
        }
        self.remote.draft = text.to_string();
    }

    fn settle_remote_ok(&mut self, done: &RemoteDone) {
        let (id, message, bad) = remote_settle(done);
        let id = if self.activity.is_loading(id) {
            id
        } else if self.activity.is_loading("askpass_pending") {
            "askpass_pending"
        } else if self.activity.is_loading("answer_askpass") {
            "answer_askpass"
        } else {
            id
        };
        if bad {
            self.activity.fail(id, message);
        } else {
            self.activity.succeed(id, message);
        }
    }

    pub fn accept_remote(&mut self, done: RemoteDone) -> Effect {
        self.settle_remote_ok(&done);
        match done {
            RemoteDone::Fetched(status) => {
                self.remote.note_fetch(status);
                Effect::Refresh
            }
            RemoteDone::Branches(branches) => {
                self.remote.set_branches(branches);
                self.remote.setup_busy = false;
                Effect::None
            }
            RemoteDone::Target { target_ref } => {
                if let Some(workspace) = &mut self.workspace {
                    Arc::make_mut(workspace).target_branch = Some(target_ref);
                }
                self.remote.setup_busy = false;
                self.remote.setup_error = None;
                self.remote.push_remote_focused = false;
                self.sync_hints();
                Effect::Refresh
            }
            RemoteDone::Added(project) => {
                if !self
                    .projects
                    .iter()
                    .any(|existing| existing.id == project.id)
                {
                    self.projects.push(project.clone());
                }
                self.remote.add_open = false;
                self.remote.add_focused = false;
                self.remote.add_path.clear();
                self.project_menu = false;
                Effect::SelectProject(project.id)
            }
            RemoteDone::Preview(preview) => {
                self.remote.preview_busy = false;
                if !preview.worktree_conflicts.is_empty() {
                    self.remote.integrate_error = Some(format!(
                        "Uncommitted changes conflict: {}",
                        preview.worktree_conflicts.join(", ")
                    ));
                }
                self.remote.preview = Some(preview);
                Effect::None
            }
            RemoteDone::Applied(report) => {
                self.bump_data();
                self.workspace = Some(Arc::new(report.workspace));
                self.remote.integrate_busy.clear();
                self.remote.preview_busy = false;
                self.remote.integrate_confirm = false;
                self.remote.integrate_open = false;
                self.remote.upstream_count = 0;
                self.remote.upstream.clear();
                self.remote.fetch_summary = Some("Up to date".to_string());
                if !report.worktree_conflicts.is_empty() {
                    self.remote.integrate_error = Some(format!(
                        "Some uncommitted changes conflicted: {}",
                        report.worktree_conflicts.join(", ")
                    ));
                    self.toast_bad = true;
                    self.toast = self.remote.integrate_error.clone();
                } else {
                    self.remote.integrate_error = None;
                    self.toast_bad = false;
                    self.toast = Some("Upstream commits are now part of your lanes.".to_string());
                }
                self.sync_hints();
                self.clamp();
                Effect::Refresh
            }
            RemoteDone::Pushed { lane, text } => {
                self.remote.push_busy = None;
                self.remote.push_results.insert(lane, text.clone());
                self.toast_bad = text.to_ascii_lowercase().contains("rejected")
                    || text.to_ascii_lowercase().contains("did not");
                self.toast = Some(text);
                Effect::Refresh
            }
            RemoteDone::Answered => {
                self.remote.submitting = false;
                Effect::None
            }
        }
    }

    pub fn fail_remote(&mut self, op: &RemoteOp, message: String) {
        let (id, _) = remote_activity(op);
        self.activity.fail(id, message.clone());
        self.toast_bad = true;
        self.toast = Some(message.clone());
        match op {
            RemoteOp::Fetch => {
                self.remote.fetching = false;
                self.remote.fetch_error = Some(message);
            }
            RemoteOp::LoadBranches | RemoteOp::SetTarget { .. } => {
                self.remote.setup_busy = false;
                self.remote.setup_error = Some(message);
            }
            RemoteOp::AddProject { .. } => {
                self.remote.setup_error = Some(message);
            }
            RemoteOp::Preview { .. } => {
                self.remote.preview_busy = false;
                self.remote.integrate_error = Some(message);
            }
            RemoteOp::Apply { .. } => {
                self.remote.integrate_busy.clear();
                self.remote.integrate_error = Some(message);
            }
            RemoteOp::Push { lane, .. } => {
                self.remote.push_busy = None;
                let text = plain_push_rejection(&message);
                self.remote.push_results.insert(lane.clone(), text.clone());
                self.toast = Some(text);
            }
            RemoteOp::Answer { .. } => {
                self.abandon_askpass();
                if message.to_ascii_lowercase().contains("timed out")
                    || message.to_ascii_lowercase().contains("no longer waiting")
                {
                    self.toast = Some("The sign-in prompt expired.".to_string());
                }
            }
        }
    }

    /// Keys for setup, fetch, integrate, push, and askpass.
    /// `None` lets the rest of the window handle the key.
    pub fn apply_remote_key(&mut self, key: &str) -> Option<Effect> {
        if self.remote.askpass.is_some() {
            return Some(self.askpass_key(key));
        }
        if self.help {
            return None;
        }
        if self.remote.push_confirm.is_some() {
            return Some(self.push_confirm_key(key));
        }
        if self.remote.integrate_confirm {
            return Some(self.integrate_confirm_key(key));
        }
        if self.remote.push_menu.is_some() {
            return Some(self.push_menu_key(key));
        }
        if self.remote.integrate_open {
            if matches!(key, "cmd-p" | "cmd-P") {
                self.remote.integrate_open = false;
                return None;
            }
            return Some(self.integrate_key(key));
        }
        if self.remote.add_focused {
            return Some(self.add_field_key(key));
        }
        if self.needs_setup() && !self.project_menu {
            return self.setup_key(key);
        }
        if matches!(key, "cmd-shift-f" | "cmd-shift-F") {
            return Some(self.request_fetch());
        }
        None
    }

    pub fn unpushed_count(&self, lane: &str) -> usize {
        self.workspace
            .as_ref()
            .and_then(|workspace| {
                workspace.stacks.iter().find_map(|stack| {
                    stack
                        .branches
                        .iter()
                        .find(|branch| branch.name == lane)
                        .map(|branch| {
                            branch
                                .commits
                                .iter()
                                .filter(|commit| !commit.pushed)
                                .count()
                        })
                })
            })
            .unwrap_or(0)
    }

    pub(crate) fn ensure_lane(&self, lane: &str) -> Result<(), String> {
        if self.lane_names().iter().any(|name| name == lane) {
            Ok(())
        } else {
            Err(format!("No lane named {lane}"))
        }
    }

    fn askpass_key(&mut self, key: &str) -> Effect {
        match key {
            "enter" => self.submit_askpass(),
            "escape" => self.cancel_askpass(),
            "tab" => {
                self.remote.reveal = !self.remote.reveal;
                Effect::None
            }
            other => {
                edit_line(&mut self.remote.draft, other);
                Effect::None
            }
        }
    }

    fn push_confirm_key(&mut self, key: &str) -> Effect {
        match key {
            "enter" => {
                let Some(ask) = self.remote.push_confirm.clone() else {
                    return Effect::None;
                };
                self.push_now(&ask.lane, ask.force).unwrap_or_else(|error| {
                    self.toast_bad = true;
                    self.toast = Some(error);
                    Effect::None
                })
            }
            "escape" => {
                self.remote.push_confirm = None;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn integrate_confirm_key(&mut self, key: &str) -> Effect {
        match key {
            "enter" => self.confirm_integrate(),
            "escape" => {
                self.remote.integrate_confirm = false;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn push_menu_key(&mut self, key: &str) -> Effect {
        let len = PUSH_CHOICES.len();
        match key {
            "up" => {
                if self.remote.push_cursor > 0 {
                    self.remote.push_cursor -= 1;
                }
                Effect::None
            }
            "down" => {
                if self.remote.push_cursor + 1 < len {
                    self.remote.push_cursor += 1;
                }
                Effect::None
            }
            "enter" => self.choose_push(),
            "escape" => {
                self.remote.push_menu = None;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn choose_push(&mut self) -> Effect {
        let Some(lane) = self.remote.push_menu.clone() else {
            return Effect::None;
        };
        let force = PUSH_CHOICES
            .get(self.remote.push_cursor)
            .map(|(_, force)| *force)
            .unwrap_or(ForcePush::No);
        if force == ForcePush::No {
            return self.push_now(&lane, force).unwrap_or_else(|error| {
                self.toast_bad = true;
                self.toast = Some(error);
                Effect::None
            });
        }
        if let Err(error) = self.ask_force_push(&lane, force) {
            self.toast_bad = true;
            self.toast = Some(error);
        }
        Effect::None
    }

    fn integrate_key(&mut self, key: &str) -> Effect {
        let lanes = self.lane_names();
        let len = lanes.len() + 1;
        match key {
            "up" => {
                if self.remote.integrate_cursor > 0 {
                    self.remote.integrate_cursor -= 1;
                }
                Effect::None
            }
            "down" => {
                if len > 0 && self.remote.integrate_cursor + 1 < len {
                    self.remote.integrate_cursor += 1;
                }
                Effect::None
            }
            "enter" => {
                if self.remote.integrate_cursor >= lanes.len() {
                    self.ask_integrate();
                    return Effect::None;
                }
                let Some(lane) = lanes.get(self.remote.integrate_cursor).cloned() else {
                    return Effect::None;
                };
                let next = match self.remote.strategy_of(&lane) {
                    UpstreamStrategy::Rebase => UpstreamStrategy::Merge,
                    UpstreamStrategy::Merge => UpstreamStrategy::Rebase,
                };
                self.remote.strategies.insert(lane, next);
                self.repreview()
            }
            "escape" => {
                self.remote.integrate_open = false;
                self.remote.integrate_confirm = false;
                self.remote.preview_busy = false;
                Effect::None
            }
            "cmd-shift-f" | "cmd-shift-F" => self.request_fetch(),
            _ => Effect::None,
        }
    }

    fn repreview(&mut self) -> Effect {
        let lanes = self.lane_names();
        match self.remote.planned_strategy(&lanes) {
            Ok(strategy) => {
                self.remote.integrate_error = None;
                self.remote.preview_busy = true;
                Effect::Remote(RemoteOp::Preview { strategy })
            }
            Err(error) => {
                self.remote.preview_busy = false;
                self.remote.integrate_error = Some(error);
                Effect::None
            }
        }
    }

    fn add_field_key(&mut self, key: &str) -> Effect {
        match key {
            "enter" => {
                let path = self.remote.add_path.clone();
                self.request_add(path)
            }
            "escape" => {
                self.remote.add_focused = false;
                Effect::None
            }
            "up" | "down" | "tab" => Effect::None,
            other => {
                edit_line(&mut self.remote.add_path, other);
                Effect::None
            }
        }
    }

    fn setup_key(&mut self, key: &str) -> Option<Effect> {
        if self.remote.push_remote_focused {
            return Some(match key {
                "enter" => {
                    self.remote.push_remote_focused = false;
                    self.request_selected_target()
                }
                "escape" | "tab" => {
                    self.remote.push_remote_focused = false;
                    Effect::None
                }
                other => {
                    self.remote.push_remote_edited = true;
                    edit_line(&mut self.remote.push_remote, other);
                    Effect::None
                }
            });
        }
        match key {
            "up" => {
                self.remote.move_branch(-1);
                Some(Effect::None)
            }
            "down" => {
                self.remote.move_branch(1);
                Some(Effect::None)
            }
            "enter" => Some(self.request_selected_target()),
            "cmd-shift-f" | "cmd-shift-F" => Some(self.request_fetch()),
            "cmd-p" | "cmd-P" | "cmd-r" | "cmd-R" => None,
            _ => Some(Effect::None),
        }
    }

    pub fn integrate_message(&self) -> String {
        let summary = self
            .remote
            .preview
            .as_ref()
            .map(|preview| preview.summary.clone())
            .unwrap_or_else(|| {
                self.remote
                    .upstream_chip()
                    .unwrap_or_else(|| "Update your lanes.".to_string())
            });
        format!("{summary} Lanes marked Conflicts stay conflicted until you fix them.")
    }
}

pub const PUSH_CHOICES: &[(&str, ForcePush)] = &[
    ("Push", ForcePush::No),
    ("Force push with lease", ForcePush::WithLease),
    ("Force push", ForcePush::Force),
];

pub fn push_choice_tip(force: ForcePush, lane: &str) -> String {
    match force {
        ForcePush::No => format!("Push lane {lane} to origin"),
        ForcePush::WithLease => {
            format!("Force push lane {lane} only if the remote still matches your last fetch")
        }
        ForcePush::Force => format!("Force push lane {lane}, overwriting other commits on origin"),
    }
}

pub fn askpass_label(request: &AskpassRequest) -> &'static str {
    match request.kind {
        AskpassKind::Passphrase => "Passphrase",
        AskpassKind::Credential => {
            let lower = request.prompt.to_ascii_lowercase();
            if lower.contains("token") {
                "Token"
            } else if lower.contains("username") && !lower.contains("password") {
                "Username"
            } else {
                "Password"
            }
        }
        AskpassKind::Other => "Response",
    }
}

pub fn mask_secret(secret: &str, reveal: bool) -> String {
    if reveal {
        secret.to_string()
    } else {
        "•".repeat(secret.chars().count())
    }
}

pub fn describe_push(report: &PushReport, unpushed: usize) -> String {
    let summary = report.summary.trim();
    let nothing = summary.eq_ignore_ascii_case("nothing to push")
        || summary.to_ascii_lowercase().contains("nothing to push");
    if nothing || report.branches.is_empty() {
        return "Nothing to push".to_string();
    }
    match unpushed {
        0 => {
            if summary.is_empty() {
                "Pushed".to_string()
            } else {
                summary.to_string()
            }
        }
        1 => "Pushed 1 commit".to_string(),
        count => format!("Pushed {count} commits"),
    }
}

pub fn plain_push_rejection(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rejected")
        || lower.contains("does not")
        || lower.contains("did not finish")
        || lower.contains("no lane")
        || lower.contains("not found")
        || lower.contains("password")
        || lower.contains("askpass")
    {
        return message.to_string();
    }
    format!("The push did not finish. {message}")
}

pub fn perform_remote(
    backend: &dyn Backend,
    project_id: Option<&str>,
    op: &RemoteOp,
    secret: Option<String>,
) -> Result<RemoteDone, String> {
    let need_project = !matches!(op, RemoteOp::Answer { .. } | RemoteOp::AddProject { .. });
    let id = if need_project {
        project_id.ok_or_else(|| "Choose a project first.".to_string())?
    } else {
        ""
    };
    match op {
        RemoteOp::Fetch => {
            let report = backend.fetch(id).map_err(bridge_message)?;
            Ok(RemoteDone::Fetched(report.status))
        }
        RemoteOp::LoadBranches => {
            let branches = backend.remote_branches(id).map_err(bridge_message)?;
            Ok(RemoteDone::Branches(branches))
        }
        RemoteOp::SetTarget {
            target_ref,
            push_remote,
        } => {
            backend
                .set_target(id, target_ref, push_remote.as_deref())
                .map_err(bridge_message)?;
            Ok(RemoteDone::Target {
                target_ref: target_ref.clone(),
            })
        }
        RemoteOp::AddProject { path } => {
            let project = backend.add_project(path).map_err(bridge_message)?;
            Ok(RemoteDone::Added(project))
        }
        RemoteOp::Preview { strategy } => {
            let preview = backend
                .integrate_preview(id, *strategy)
                .map_err(bridge_message)?;
            Ok(RemoteDone::Preview(preview))
        }
        RemoteOp::Apply { strategy } => {
            let report = backend.integrate(id, *strategy).map_err(bridge_message)?;
            Ok(RemoteDone::Applied(report))
        }
        RemoteOp::Push {
            lane,
            force,
            unpushed,
        } => {
            let report = backend.push(id, lane, *force).map_err(bridge_message)?;
            Ok(RemoteDone::Pushed {
                lane: lane.clone(),
                text: describe_push(&report, *unpushed),
            })
        }
        RemoteOp::Answer { id, cancel } => {
            let response = if *cancel { None } else { secret };
            backend
                .answer_askpass(id, response)
                .map_err(bridge_message)?;
            Ok(RemoteDone::Answered)
        }
    }
}

fn bridge_message(error: gb_bridge::BridgeError) -> String {
    match error.hint {
        Some(hint) if !hint.is_empty() => format!("{} {hint}", error.message),
        _ => error.message,
    }
}

fn format_fetched(ms: u128) -> String {
    let Ok(ms) = i64::try_from(ms) else {
        return "earlier".to_string();
    };
    let Some(time) = chrono::DateTime::from_timestamp_millis(ms) else {
        return "earlier".to_string();
    };
    time.with_timezone(&chrono::Local)
        .format("%b %d, %H:%M")
        .to_string()
}

fn edit_line(buffer: &mut String, key: &str) {
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
    use crate::store::{Store, load};
    use gb_bridge::{FakeAskpass, FakeBackend, IntegrateResolution};

    const TOKEN: &str = "s3cret-token";

    fn ready() -> (FakeBackend, Store) {
        let backend = FakeBackend::demo();
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        (backend, store)
    }

    fn run(backend: &FakeBackend, store: &mut Store, effect: Effect) -> Effect {
        let Effect::Remote(op) = effect else {
            return effect;
        };
        let secret = if matches!(op, RemoteOp::Answer { cancel: false, .. }) {
            store.take_askpass_secret()
        } else if matches!(op, RemoteOp::Answer { cancel: true, .. }) {
            store.abandon_askpass();
            None
        } else {
            None
        };
        match perform_remote(backend, store.selected.as_deref(), &op, secret) {
            Ok(done) => store.accept_remote(done),
            Err(error) => {
                store.fail_remote(&op, error);
                Effect::None
            }
        }
    }

    #[test]
    fn setup_picks_a_remote_branch_and_a_push_remote() {
        let backend = FakeBackend::demo();
        let added = backend.add_project("/tmp/needs-setup").unwrap();
        let mut store = Store::default();
        store.apply(load(&backend, None));
        store.select(Some(added.id.clone()));
        store.apply(load(&backend, Some(&added.id)));
        assert!(store.needs_setup());
        assert_eq!(
            store.project_hint(&added.id).as_deref(),
            Some("Needs setup")
        );

        let effect = store.request_branches();
        assert_eq!(run(&backend, &mut store, effect), Effect::None);
        assert_eq!(store.remote.branches.len(), 2);
        assert_eq!(
            store.remote.selected_branch().unwrap().full_name,
            "refs/remotes/origin/main"
        );
        assert_eq!(store.remote.push_remote, "origin");
        store.apply_key("down");
        assert_eq!(store.remote.selected_branch().unwrap().branch, "topic");
        store.remote.push_remote_focused = true;
        for _ in 0..6 {
            store.apply_key("backspace");
        }
        for ch in ["u", "p", "s", "t", "r", "e", "a", "m"] {
            store.apply_key(ch);
        }
        assert_eq!(store.remote.push_remote, "upstream");
        assert!(store.remote.push_remote_edited);

        let effect = store.request_selected_target();
        assert!(matches!(effect, Effect::Remote(RemoteOp::SetTarget { .. })));
        assert_eq!(run(&backend, &mut store, effect), Effect::Refresh);
        assert!(!store.needs_setup());
        assert_eq!(
            store.workspace.as_ref().unwrap().target_branch.as_deref(),
            Some("refs/remotes/origin/topic")
        );

        store.open_add_field();
        assert_eq!(store.request_add("  ".to_string()), Effect::None);
        assert!(store.remote.setup_error.is_some());
    }

    #[test]
    fn fetch_status_moves_from_not_fetched_to_upstream_commits() {
        let (backend, mut store) = ready();
        assert_eq!(store.remote.status_line(), "Not fetched yet");
        assert!(store.remote.fetch_tip().contains("Not fetched yet"));
        assert!(store.remote.upstream_chip().is_none());

        let effect = store.apply_key("cmd-shift-f");
        assert!(matches!(effect, Effect::Remote(RemoteOp::Fetch)));
        assert!(store.remote.fetching);
        assert_eq!(run(&backend, &mut store, effect), Effect::Refresh);
        assert!(!store.remote.fetching);
        assert_eq!(store.remote.upstream_count, 2);
        assert_eq!(store.remote.status_line(), "2 new upstream commits");
        assert_eq!(
            store.remote.upstream_chip().as_deref(),
            Some("2 new upstream commits")
        );
        assert!(store.remote.last_fetched_ms.is_some());
        assert!(store.remote.fetch_tip().contains("Last fetched"));
        assert_eq!(
            store
                .remote
                .upstream
                .iter()
                .map(|commit| commit.title.as_str())
                .collect::<Vec<_>>(),
            ["Upstream one", "Upstream two"]
        );
    }

    #[test]
    fn integrate_plan_previews_each_lane_and_refuses_a_mix() {
        let (backend, mut store) = ready();
        let effect = store.request_fetch();
        run(&backend, &mut store, effect);
        let effect = store.open_integrate();
        assert!(matches!(
            effect,
            Effect::Remote(RemoteOp::Preview {
                strategy: UpstreamStrategy::Rebase
            })
        ));
        assert_eq!(run(&backend, &mut store, effect), Effect::None);
        assert!(store.remote.integrate_open);
        assert_eq!(store.remote.integrate_cursor, 0);
        assert_eq!(store.remote.lane_outcome("lane-a"), "Clean");
        assert_eq!(store.remote.lane_outcome("lane-b"), "Conflicts");
        assert_eq!(store.remote.lane_outcome("lane-c"), "Clean");
        let preview = store.remote.preview.clone().unwrap();
        assert!(
            preview
                .resolutions
                .iter()
                .any(|lane| lane.resolution == IntegrateResolution::Conflicted)
        );

        store.remote.integrate_cursor = 0;
        let toggled = store.apply_key("enter");
        assert_eq!(toggled, Effect::None);
        assert!(store.remote.integrate_error.is_some());
        assert!(
            store
                .remote
                .integrate_error
                .as_deref()
                .unwrap()
                .contains("same way")
        );

        let lanes = store.lane_names();
        for lane in lanes {
            store
                .remote
                .strategies
                .insert(lane, UpstreamStrategy::Rebase);
        }
        store.remote.integrate_error = None;
        store.apply_key("down");
        store.apply_key("down");
        store.apply_key("down");
        store.ask_integrate();
        assert!(store.remote.integrate_confirm);
        let effect = store.confirm_integrate();
        assert!(matches!(
            effect,
            Effect::Remote(RemoteOp::Apply {
                strategy: UpstreamStrategy::Rebase
            })
        ));
        assert_eq!(store.remote.integrate_busy.len(), 3);
        assert_eq!(run(&backend, &mut store, effect), Effect::Refresh);
        assert!(!store.remote.integrate_open);
        assert!(store.remote.integrate_busy.is_empty());
        assert_eq!(store.remote.upstream_count, 0);
        assert_eq!(store.remote.status_line(), "Up to date");
    }

    #[test]
    fn push_menu_confirms_force_and_maps_results() {
        let (backend, mut store) = ready();
        store.open_push_menu("lane-c").unwrap();
        assert_eq!(store.remote.push_menu.as_deref(), Some("lane-c"));
        assert_eq!(store.apply_key("down"), Effect::None);
        assert_eq!(store.remote.push_cursor, 1);
        assert_eq!(store.apply_key("enter"), Effect::None);
        let ask = store.remote.push_confirm.clone().unwrap();
        assert_eq!(ask.force, ForcePush::WithLease);
        assert!(ask.title.contains("lease"));
        store.apply_key("escape");
        assert!(store.remote.push_confirm.is_none());

        store.open_push_menu("lane-c").unwrap();
        store.remote.push_cursor = 2;
        store.apply_key("enter");
        assert_eq!(
            store.remote.push_confirm.as_ref().unwrap().force,
            ForcePush::Force
        );
        store.remote.push_confirm = None;

        let effect = store.push_now("lane-c", ForcePush::No).unwrap();
        assert!(matches!(
            effect,
            Effect::Remote(RemoteOp::Push {
                unpushed: 2,
                force: ForcePush::No,
                ..
            })
        ));
        assert_eq!(run(&backend, &mut store, effect), Effect::Refresh);
        assert_eq!(
            store.remote.push_results.get("lane-c").map(String::as_str),
            Some("Pushed 2 commits")
        );
        assert_eq!(store.toast.as_deref(), Some("Pushed 2 commits"));
        assert!(!store.toast_bad);

        let again = store.push_now("lane-c", ForcePush::No).unwrap();
        run(&backend, &mut store, again);
        assert_eq!(
            store.remote.push_results.get("lane-c").map(String::as_str),
            Some("Nothing to push")
        );

        let rejected = "The push was rejected. The remote has commits this lane does not.";
        assert_eq!(plain_push_rejection(rejected), rejected);
        store.fail_remote(
            &RemoteOp::Push {
                lane: "lane-a".to_string(),
                force: ForcePush::No,
                unpushed: 0,
            },
            rejected.to_string(),
        );
        assert_eq!(
            store.remote.push_results.get("lane-a").map(String::as_str),
            Some(rejected)
        );
        assert!(store.open_push_menu("missing").is_err());
    }

    #[test]
    fn askpass_masks_cancels_and_hides_the_secret_from_debug() {
        let (backend, mut store) = ready();
        backend.trigger_fake("demo-project", "askpass").unwrap();
        let pending = backend.askpass_pending().unwrap();
        assert_eq!(pending.len(), 1);
        store.open_askpass(pending[0].clone());
        assert_eq!(askpass_label(&pending[0]), "Password");
        for ch in ["s", "3", "c", "r", "e", "t", "-", "t", "o", "k", "e", "n"] {
            store.apply_key(ch);
        }
        assert_eq!(store.remote.shown_secret(), "•".repeat(TOKEN.len()));
        store.apply_key("tab");
        assert_eq!(store.remote.shown_secret(), TOKEN);
        store.apply_key("tab");
        assert_eq!(mask_secret(TOKEN, false), "•".repeat(TOKEN.len()));

        let dumped = format!("{store:?}");
        let flags = store.remote_state_line();
        assert!(!dumped.contains(TOKEN));
        assert!(dumped.contains("[redacted]"));
        assert!(!flags.contains(TOKEN));
        assert!(flags.contains("askpass_open=true"));

        let effect = store.cancel_askpass();
        assert!(matches!(
            effect,
            Effect::Remote(RemoteOp::Answer { cancel: true, .. })
        ));
        run(&backend, &mut store, effect);
        assert!(backend.askpass_pending().unwrap().is_empty());
        assert!(store.remote.askpass.is_none());
        assert_eq!(store.remote.shown_secret(), "");
        assert!(!format!("{store:?}").contains(TOKEN));

        backend.trigger_fake("demo-project", "askpass").unwrap();
        let pending = backend.askpass_pending().unwrap();
        store.open_askpass(pending[0].clone());
        store.set_askpass_text(TOKEN);
        let effect = store.submit_askpass();
        run(&backend, &mut store, effect);
        assert!(backend.askpass_pending().unwrap().is_empty());
        assert!(store.remote.askpass.is_none());
        assert!(!format!("{store:?}").contains(TOKEN));
        assert!(!store.remote_state_line().contains(TOKEN));

        backend.trigger_fake("demo-project", "askpass").unwrap();
        let pending = backend.askpass_pending().unwrap();
        store.open_askpass(pending[0].clone());
        let mut scripted = FakeAskpass::scripted([Some(TOKEN.to_string())]);
        assert_eq!(scripted.respond(&backend).unwrap(), 1);
        store.note_askpass_pending(&backend.askpass_pending().unwrap());
        assert!(store.remote.askpass.is_none());
        assert_eq!(store.toast.as_deref(), Some("The sign-in prompt expired."));
        assert!(!format!("{store:?}").contains(TOKEN));

        let passphrase = AskpassRequest {
            id: "phrase".to_string(),
            prompt: "Enter passphrase for key: ".to_string(),
            kind: AskpassKind::Passphrase,
        };
        assert_eq!(askpass_label(&passphrase), "Passphrase");
        let token = AskpassRequest {
            id: "tok".to_string(),
            prompt: "Token for 'https://example.test': ".to_string(),
            kind: AskpassKind::Credential,
        };
        assert_eq!(askpass_label(&token), "Token");
    }

    #[test]
    fn fake_backend_fetch_integrate_push_and_askpass_round_trip() {
        let (backend, mut store) = ready();
        assert_eq!(
            backend.fetch_status("demo-project").unwrap().upstream_count,
            0
        );
        let fetched = store.request_fetch();
        run(&backend, &mut store, fetched);
        assert_eq!(store.remote.status_line(), "2 new upstream commits");

        let preview = store.open_integrate();
        run(&backend, &mut store, preview);
        assert_eq!(store.remote.lane_outcome("lane-c"), "Clean");
        assert_eq!(store.remote.lane_outcome("lane-b"), "Conflicts");
        let apply = store.confirm_integrate();
        run(&backend, &mut store, apply);
        assert_eq!(
            backend.fetch_status("demo-project").unwrap().upstream_count,
            0
        );
        assert_eq!(store.remote.status_line(), "Up to date");

        // The applied workspace is the bridge image. Re-read so push counts match.
        store.workspace = Some(Arc::new(backend.open_project("demo-project").unwrap()));
        let push = store.push_now("lane-c", ForcePush::No).unwrap();
        run(&backend, &mut store, push);
        assert_eq!(
            store.remote.push_results.get("lane-c").map(String::as_str),
            Some("Pushed 2 commits")
        );
        let after = backend.open_project("demo-project").unwrap();
        let lane_c = after
            .stacks
            .iter()
            .flat_map(|stack| &stack.branches)
            .find(|branch| branch.name == "lane-c")
            .unwrap();
        assert!(lane_c.commits.iter().all(|commit| commit.pushed));

        backend.trigger_fake("demo-project", "askpass").unwrap();
        let pending = backend.askpass_pending().unwrap();
        store.open_askpass(pending[0].clone());
        store.set_askpass_text(TOKEN);
        let effect = store.submit_askpass();
        assert_eq!(run(&backend, &mut store, effect), Effect::None);
        assert!(backend.askpass_pending().unwrap().is_empty());
        assert!(store.remote.askpass.is_none());
        assert_eq!(store.remote.shown_secret(), "");

        backend.trigger_fake("demo-project", "askpass").unwrap();
        let pending = backend.askpass_pending().unwrap();
        store.open_askpass(pending[0].clone());
        let mut scripted = FakeAskpass::scripted([None]);
        assert_eq!(scripted.respond(&backend).unwrap(), 1);
        store.note_askpass_pending(&backend.askpass_pending().unwrap());
        assert!(store.remote.askpass.is_none());
        assert_eq!(store.remote.shown_secret(), "");
        assert!(!format!("{store:?}").contains(TOKEN));
        assert!(!store.remote_state_line().contains(TOKEN));
    }
}
