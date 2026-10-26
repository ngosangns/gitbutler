//! GitButler window. `GB_GPUI_FAKE=1` serves canned data and never opens the
//! installed app's files. A live window runs GitButler's watcher, which writes
//! hunk assignments the same way GitButler does.
//!
//! ns-ui has no GitButler tint. The accent is Hearth blue until one is added there.

mod audit;
#[cfg(test)]
mod bench;
mod perf_harness;
mod store;

use perf_harness::{PerfRun, RenderClock};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ely_gpui_component::buttons::{ButtonVariant, IconButton};
use ely_gpui_component::layout::ScrollArea;
use ely_gpui_component::primitives::{IconName, Tooltip};
use ely_gpui_component::shell::StatusBar;
use ely_gpui_component::theme::{ActiveTheme, ControlSize, Elevation};
use gb_bridge::{
    Backend, BridgeEvent, CommitDetails, CommitFile, CommitSummary, DiffRequest, FakeBackend,
    FileChange, FileStatus, ForcePush, InProcessBackend, StackSummary,
};
use gpui::{
    App, ClickEvent, ClipboardItem, Context, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Render, ScrollHandle,
    ScrollStrategy, SharedString, Styled, UniformListScrollHandle, Window, div, point, prelude::*,
    px, uniform_list,
};
use ns_ui::Tone;
use store::{
    Action, AppPage, BranchFocusInfo, BranchTab, DiffRow, EditPhase, Effect, Field, PageOp, Pane,
    RemoteOp, RowKind, SettingsSection, ShownFile, SideFocus, Store, TreeRow, ViewMode,
    build_diff_rows, collapse_key, load, perform_settings, shortcut_rows, snapshot_kind,
    snapshot_when,
};

/// A column-edge drag. Fractions stay on the store; this only remembers the start.
struct ColumnDrag {
    handle: usize,
    origin_x: f32,
    origin: [f32; 3],
    viewport: f32,
}

fn main() {
    ns_ui::use_tint(ns_ui::Tint::Hearth);
    let backend: Arc<dyn Backend> = if std::env::var_os("GB_GPUI_FAKE").is_some() {
        Arc::new(FakeBackend::demo())
    } else {
        match InProcessBackend::new() {
            Ok(backend) => Arc::new(backend),
            Err(error) => {
                eprintln!("gb-gpui: {error}");
                std::process::exit(1);
            }
        }
    };
    #[cfg(feature = "devtools")]
    ns_ui::open_dev("gb-gpui", move |window, cx| {
        GbWindow::new(backend, window, cx)
    });
    #[cfg(not(feature = "devtools"))]
    ns_ui::open("gb-gpui", move |window, cx| {
        GbWindow::new(backend, window, cx)
    });
}

/// Quiet period after a branch-filter keystroke before the lists refilter.
/// The text box still shows every character at once.
const BRANCH_FILTER_DEBOUNCE_MS: u64 = 130;

/// Height of the "Changed files" pane on the history page.
const SNAPSHOT_FILES_PX: f32 = 88.0;

/// One snapshot file row plus its gap, as the old gap_2 flex column laid out.
const SNAPSHOT_FILE_PITCH_PX: f32 = 24.0;

struct GbWindow {
    focus: FocusHandle,
    watch: Option<gb_bridge::WatchHandle>,
    askpass_watch: Option<gb_bridge::AskpassWatch>,
    backend: Arc<dyn Backend>,
    preview: bool,
    store: Store,
    generation: u64,
    /// Bumped for every branch-filter keystroke so only the latest debounce fires.
    branch_filter_job: u64,
    events: Receiver<BridgeEvent>,
    event_tx: Sender<BridgeEvent>,
    askpass_rx: Receiver<gb_bridge::AskpassRequest>,
    askpass_tx: Sender<gb_bridge::AskpassRequest>,
    askpass_polling: bool,
    diff_scroll: UniformListScrollHandle,
    file_scroll: UniformListScrollHandle,
    lane_scroll: UniformListScrollHandle,
    detail_scroll: UniformListScrollHandle,
    branch_scroll: ScrollHandle,
    branch_list_scroll: UniformListScrollHandle,
    history_scroll: ScrollHandle,
    history_list_scroll: UniformListScrollHandle,
    column_drag: Option<ColumnDrag>,
    laid_out: [f32; 3],
    viewport_width: f32,
    ui_path: Option<std::path::PathBuf>,
    shown_page: AppPage,
    /// Draw every row, the way the lists worked before virtualization.
    perf_full: bool,
    booted: Instant,
    painted: Rc<Cell<u64>>,
    notifies: Rc<Cell<u64>>,
    shaped: Rc<RefCell<HashMap<u64, SharedString>>>,
    shaped_for: Cell<usize>,
    perf: Rc<RefCell<PerfRun>>,
    perf_armed: bool,
    perf_headless: Arc<Mutex<Option<serde_json::Value>>>,
}

impl GbWindow {
    fn new(backend: Arc<dyn Backend>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (event_tx, events) = mpsc::channel();
        let (askpass_tx, askpass_rx) = mpsc::channel();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let preview = backend.is_preview();
        let ui_path = store::persistent_file();
        let mut store = Store::default();
        if let Some(path) = &ui_path {
            store.column_fractions = store::load_fractions(path);
        }
        let laid_out = store::column_pixels(store.column_fractions, 1280.0);
        let mut view = Self {
            focus,
            watch: None,
            askpass_watch: None,
            backend,
            preview,
            store,
            generation: 0,
            branch_filter_job: 0,
            events,
            event_tx,
            askpass_rx,
            askpass_tx,
            askpass_polling: false,
            diff_scroll: UniformListScrollHandle::new(),
            file_scroll: UniformListScrollHandle::new(),
            lane_scroll: UniformListScrollHandle::new(),
            detail_scroll: UniformListScrollHandle::new(),
            branch_scroll: ScrollHandle::new(),
            branch_list_scroll: UniformListScrollHandle::new(),
            history_scroll: ScrollHandle::new(),
            history_list_scroll: UniformListScrollHandle::new(),
            column_drag: None,
            laid_out,
            viewport_width: 1280.0,
            ui_path,
            shown_page: AppPage::Workspace,
            perf_full: std::env::var_os("GB_GPUI_PERF_FULL").is_some(),
            booted: Instant::now(),
            painted: Rc::new(Cell::new(0)),
            notifies: Rc::new(Cell::new(0)),
            shaped: Rc::new(RefCell::new(HashMap::new())),
            shaped_for: Cell::new(0),
            perf: Rc::new(RefCell::new(PerfRun::default())),
            perf_armed: false,
            perf_headless: Arc::new(Mutex::new(None)),
        };
        view.pump(cx);
        view.ensure_askpass(cx);
        view.refresh(cx);
        view
    }

    fn pump(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                this.update(cx, |this, cx| {
                    let mut saw = false;
                    let now = this.elapsed_ms();
                    while let Ok(event) = this.events.try_recv() {
                        let refresh = event.refreshes_workspace();
                        this.store.apply_event(event);
                        if refresh {
                            this.store.refresh_gate.note(now);
                        }
                        saw = true;
                    }
                    while let Ok(request) = this.askpass_rx.try_recv() {
                        this.store.open_askpass(request);
                        saw = true;
                    }
                    if this.store.remote.askpass.is_some()
                        && !this.store.remote.submitting
                        && !this.askpass_polling
                    {
                        this.askpass_polling = true;
                        let backend = Arc::clone(&this.backend);
                        cx.spawn(async move |this, cx| {
                            let pending = cx
                                .background_spawn(async move { backend.askpass_pending() })
                                .await;
                            this.update(cx, |this, cx| {
                                this.askpass_polling = false;
                                if let Ok(list) = pending {
                                    this.store.note_askpass_pending(&list);
                                }
                                cx.notify();
                            })
                            .ok();
                        })
                        .detach();
                    }
                    if saw {
                        this.mark_notify();
                        cx.notify();
                    }
                    let now = this.elapsed_ms();
                    if this.store.refresh_gate.poll(now)
                        && !this.store.loading
                        && !this.store.refreshing
                        && !this.store.blocks_open()
                    {
                        this.refresh_workspace(cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn elapsed_ms(&self) -> u64 {
        self.booted.elapsed().as_millis() as u64
    }

    fn mark_notify(&self) {
        self.notifies.set(self.notifies.get().saturating_add(1));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.store.refresh_gate.cancel();
        self.generation += 1;
        let generation = self.generation;
        if self.store.workspace.is_some() || self.store.worktree.is_some() {
            self.store.refreshing = true;
        } else {
            self.store.loading = true;
        }
        self.begin_reads();
        cx.notify();
        let backend = Arc::clone(&self.backend);
        let selected = self.store.selected.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { load(backend.as_ref(), selected.as_deref()) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    this.abandon_reads();
                    return;
                }
                this.store.apply(loaded);
                if this.store.selected.is_none()
                    && !this.store.blocks_open()
                    && let Some(first) = this.store.projects.first()
                {
                    this.store.select(Some(first.id.clone()));
                    cx.notify();
                    this.refresh(cx);
                    return;
                }
                this.ensure_watch(cx);
                this.sync_remote_snapshot(cx);
                this.maybe_start_env_perf(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn settle_refresh(&mut self, cx: &mut Context<Self>) {
        self.store.refreshing = false;
        let now = self.elapsed_ms();
        self.store.refresh_gate.finish(now);
        if self.store.refresh_gate.poll(now)
            && !self.store.loading
            && !self.store.blocks_open()
            && self.store.selected.is_some()
        {
            self.refresh_workspace(cx);
        }
    }

    fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.store.selected.clone() else {
            self.store.workspace_stale = false;
            self.store.refresh_gate.finish(self.elapsed_ms());
            return;
        };
        self.store.workspace_stale = false;
        self.store.refreshing = true;
        self.store.activity.begin("open_project", None);
        self.store.activity.begin("refresh_status", None);
        self.generation += 1;
        let generation = self.generation;
        let backend = Arc::clone(&self.backend);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.open_project(&id) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    this.store.activity.abandon("open_project");
                    this.store.activity.abandon("refresh_status");
                    this.settle_refresh(cx);
                    return;
                }
                match result {
                    Ok(workspace) => {
                        this.store.set_workspace(Some(Arc::new(workspace)));
                        this.store.set_error(None);
                        this.store
                            .activity
                            .succeed("open_project", "The project is open.");
                        this.store
                            .activity
                            .succeed("refresh_status", "Refresh finished.");
                    }
                    Err(error) => {
                        this.store.set_error(Some(error.message.clone()));
                        this.store
                            .activity
                            .fail("open_project", error.message.clone());
                        this.store.activity.fail("refresh_status", error.message);
                    }
                }
                this.store.last_refresh = Some(SystemTime::now());
                this.sync_remote_snapshot(cx);
                this.settle_refresh(cx);
                this.mark_notify();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_project(&mut self, id: String, cx: &mut Context<Self>) {
        if self.store.blocks_open() {
            return;
        }
        self.store.set_project_menu(false);
        self.watch = None;
        self.store.set_watch(None);
        self.store.select(Some(id));
        self.refresh(cx);
    }

    fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        if self.store.blocks_open() || self.store.selected.is_none() {
            self.watch = None;
            self.store.set_watch(None);
            return;
        }
        let Some(id) = self.store.selected.clone() else {
            return;
        };
        self.watch = None;
        self.store.set_watch(None);
        let backend = Arc::clone(&self.backend);
        let tx = self.event_tx.clone();
        let watched_id = id.clone();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_spawn(async move {
                    backend.watch(
                        &watched_id,
                        Box::new(move |event| {
                            let _ = tx.send(event);
                        }),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if this.store.selected.as_deref() != Some(id.as_str()) {
                    return;
                }
                match started {
                    Ok(handle) => {
                        let exclusive = handle.is_exclusive();
                        this.watch = Some(handle);
                        this.store.set_watch(Some(exclusive));
                    }
                    Err(error) => {
                        this.watch = None;
                        this.store.set_watch(None);
                        this.store.set_error(Some(error.message));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn begin_reads(&mut self) {
        for id in [
            "list_projects",
            "open_project",
            "worktree",
            "refresh_status",
            "forge_status",
        ] {
            self.store.activity.begin(id, None);
        }
    }

    fn abandon_reads(&mut self) {
        for id in [
            "list_projects",
            "open_project",
            "worktree",
            "refresh_status",
            "forge_status",
        ] {
            self.store.activity.abandon(id);
        }
    }

    /// Preview work stays on this thread only when the fake delay is zero.
    /// A delay uses the background path so the dev command returns while the
    /// spinner is still up.
    fn preview_immediate(&self) -> bool {
        self.preview && self.backend.preview_latency_ms() == 0
    }

    fn activate(&mut self, effect: Effect, cx: &mut Context<Self>) {
        if !matches!(effect, Effect::Refresh | Effect::SelectProject(_)) {
            if let Some((id, row)) = Store::effect_activity(&effect) {
                self.store.activity.begin(id, row);
            }
            if matches!(effect, Effect::Settings(store::SettingsOp::Load)) {
                self.store.activity.begin("read_project", None);
            }
        }
        match effect {
            Effect::None => {}
            Effect::Refresh => self.refresh(cx),
            Effect::SelectProject(id) => self.select_project(id, cx),
            Effect::ShowCommit(sha) => self.load_commit(sha, cx),
            Effect::ShowDiff(request) => self.load_diff(request, cx),
            Effect::Mutate(action) => self.run_action(action, cx),
            Effect::Remote(op) => self.run_remote(op, cx),
            Effect::Page(op) => self.run_page(op, cx),
            Effect::Forge(op) => self.run_forge(op, cx),
            Effect::Suggest(paths) => self.run_suggest(paths, cx),
            Effect::Settings(op) => self.run_settings(op, cx),
        }
    }

    fn run_forge(&mut self, op: store::ForgeOp, cx: &mut Context<Self>) {
        let project = self.store.selected.clone();
        let branch = match &op {
            store::ForgeOp::Publish { branch } => branch.clone(),
        };
        if self.preview_immediate() {
            let outcome = store::perform_forge(self.backend.as_ref(), project.as_deref(), &op);
            self.finish_forge(&branch, outcome, cx);
            return;
        }
        let backend = Arc::clone(&self.backend);
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    store::perform_forge(backend.as_ref(), project.as_deref(), &op)
                })
                .await;
            this.update(cx, |this, cx| {
                this.finish_forge(&branch, outcome, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_forge(
        &mut self,
        branch: &str,
        outcome: Result<store::ForgeDone, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(done) => self.store.accept_forge(done),
            Err(error) => self.store.fail_forge(branch, error),
        }
        cx.notify();
    }

    fn run_suggest(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        let project = self.store.selected.clone();
        if self.preview_immediate() {
            let outcome = store::perform_suggest(self.backend.as_ref(), project.as_deref(), &paths);
            self.finish_suggest(outcome, cx);
            return;
        }
        let backend = Arc::clone(&self.backend);
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    store::perform_suggest(backend.as_ref(), project.as_deref(), &paths)
                })
                .await;
            this.update(cx, |this, cx| {
                this.finish_suggest(outcome, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_suggest(
        &mut self,
        outcome: Result<gb_bridge::SuggestedMessage, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(message) => self.store.accept_suggestion(message),
            Err(error) => self.store.fail_suggestion(error),
        }
        cx.notify();
    }

    fn run_settings(&mut self, op: store::SettingsOp, cx: &mut Context<Self>) {
        let project = self.store.selected.clone();
        if self.preview_immediate() {
            let outcome = perform_settings(self.backend.as_ref(), project.as_deref(), &op);
            self.finish_settings(&op, outcome, cx);
            return;
        }
        let backend = Arc::clone(&self.backend);
        let failed = op.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    perform_settings(backend.as_ref(), project.as_deref(), &op)
                })
                .await;
            this.update(cx, |this, cx| {
                this.finish_settings(&failed, outcome, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_settings(
        &mut self,
        op: &store::SettingsOp,
        outcome: Result<store::SettingsDone, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(done) => self.store.accept_settings(done),
            Err(error) => self.store.fail_settings(op, error),
        }
        cx.notify();
    }

    fn run_page(&mut self, op: PageOp, cx: &mut Context<Self>) {
        let project = self.store.selected.clone();
        if self.preview_immediate() {
            let outcome = store::perform_page(self.backend.as_ref(), project.as_deref(), &op);
            self.finish_page(op, outcome, cx);
            return;
        }
        let backend = Arc::clone(&self.backend);
        let failed = op.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    store::perform_page(backend.as_ref(), project.as_deref(), &op)
                })
                .await;
            this.update(cx, |this, cx| {
                this.finish_page(failed, outcome, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_page(
        &mut self,
        op: PageOp,
        outcome: Result<store::PageDone, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(done) => {
                let effect = self.store.accept_page(done);
                self.activate(effect, cx);
            }
            Err(error) => self.store.fail_page(&op, error),
        }
        cx.notify();
    }

    fn ensure_askpass(&mut self, cx: &mut Context<Self>) {
        if self.askpass_watch.is_some() {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let tx = self.askpass_tx.clone();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_spawn(async move {
                    backend.watch_askpass(Box::new(move |request| {
                        let _ = tx.send(request);
                    }))
                })
                .await;
            this.update(cx, |this, cx| {
                if let Ok(watch) = started {
                    this.askpass_watch = Some(watch);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn sync_remote_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.store.selected.clone() else {
            return;
        };
        if self.store.needs_setup() {
            let effect = self.store.request_branches();
            self.activate(effect, cx);
        }
        if self.preview {
            self.apply_remote_snapshot(&id);
            return;
        }
        let backend = Arc::clone(&self.backend);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let status = backend.fetch_status(&id).ok();
                    (id, status)
                })
                .await;
            this.update(cx, |this, cx| {
                let (id, status) = loaded;
                if this.store.selected.as_deref() != Some(id.as_str()) {
                    return;
                }
                if let Some(status) = status
                    && !this.store.remote.fetching
                {
                    this.store.remote.note_fetch(status);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_remote_snapshot(&mut self, id: &str) {
        if self.store.remote.fetching {
            return;
        }
        if let Ok(status) = self.backend.fetch_status(id) {
            self.store.remote.note_fetch(status);
        }
    }

    fn run_remote(&mut self, op: RemoteOp, cx: &mut Context<Self>) {
        let secret = match &op {
            RemoteOp::Answer { cancel: false, .. } => self.store.take_askpass_secret(),
            RemoteOp::Answer { cancel: true, .. } => {
                self.store.abandon_askpass();
                None
            }
            _ => None,
        };
        let project = self.store.selected.clone();
        if self.preview_immediate() {
            let outcome =
                store::perform_remote(self.backend.as_ref(), project.as_deref(), &op, secret);
            self.finish_remote(op, outcome, cx);
            return;
        }
        let backend = Arc::clone(&self.backend);
        let failed = op.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    store::perform_remote(backend.as_ref(), project.as_deref(), &op, secret)
                })
                .await;
            this.update(cx, |this, cx| {
                this.finish_remote(failed, outcome, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_remote(
        &mut self,
        op: RemoteOp,
        outcome: Result<store::RemoteDone, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(done) => {
                let effect = self.store.accept_remote(done);
                self.activate(effect, cx);
            }
            Err(error) => self.store.fail_remote(&op, error),
        }
        cx.notify();
    }

    fn run_action(&mut self, action: Action, cx: &mut Context<Self>) {
        if !self.store.arm_busy(&action) {
            if let Some((id, _)) = Store::effect_activity(&Effect::Mutate(action)) {
                self.store.activity.abandon(id);
            }
            cx.notify();
            return;
        }
        let Some(id) = self.store.selected.clone() else {
            self.store.fail_busy("Choose a project first.");
            cx.notify();
            return;
        };
        if self.preview_immediate() {
            let outcome = store::perform_action(self.backend.as_ref(), &id, &action);
            self.finish_mutation(outcome, cx);
            return;
        }
        let backend = Arc::clone(&self.backend);
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(
                    async move { store::perform_action(backend.as_ref(), &id, &action) },
                )
                .await;
            this.update(cx, |this, cx| {
                this.finish_mutation(outcome, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_mutation(
        &mut self,
        outcome: Result<store::MutationDone, String>,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok(done) => {
                let effect = self.store.accept(done);
                self.activate(effect, cx);
            }
            Err(error) => self.store.fail_busy(&error),
        }
        cx.notify();
    }

    fn load_commit(&mut self, sha: String, cx: &mut Context<Self>) {
        let Some(id) = self.store.selected.clone() else {
            return;
        };
        self.store
            .activity
            .begin("commit_details", Some(sha.clone()));
        let backend = Arc::clone(&self.backend);
        let wanted = sha.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { backend.commit_details(&id, &sha) })
                .await;
            this.update(cx, |this, cx| {
                this.store
                    .set_commit(&wanted, result.map_err(|error| error.message));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_diff(&mut self, request: DiffRequest, cx: &mut Context<Self>) {
        let Some(id) = self.store.selected.clone() else {
            return;
        };
        self.store.activity.begin("file_diff", None);
        let lanes = self.store.lanes_for(&request);
        let limit = self.store.diff_limit;
        let backend = Arc::clone(&self.backend);
        let wanted = request.clone();
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_spawn(async move {
                    let diff = backend.file_diff(&id, request)?;
                    let rows = build_diff_rows(
                        &diff.path,
                        diff.binary,
                        diff.too_large,
                        &diff.patch,
                        &lanes,
                        limit,
                    );
                    Ok::<Vec<DiffRow>, gb_bridge::BridgeError>(rows)
                })
                .await;
            this.update(cx, |this, cx| {
                this.store
                    .set_diff(&wanted, built.map_err(|error| error.message));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn on_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some(token) = key_token(keystroke) else {
            return;
        };
        let before = self.store.app_page();
        self.capture_scroll();
        let (effect, handled) = self.store.apply_key_handled(&token);
        if self.store.app_page() != before {
            self.restore_scroll();
            self.shown_page = self.store.app_page();
        }
        self.activate(effect, cx);
        self.scroll_lists();
        if self.store.branch_filter_pending() {
            self.schedule_branch_refilter(cx);
        }
        // An unhandled key changed no state; notify would redraw the whole
        // window for nothing, and the keystroke still reaches OS hotkeys.
        if handled {
            cx.notify();
        }
    }

    /// Queues a branch-list refilter after the filter goes quiet. A newer
    /// keystroke bumps the job and the older timer does nothing. Enter, tab
    /// changes, and dev commands commit the filter at once in the store.
    fn schedule_branch_refilter(&mut self, cx: &mut Context<Self>) {
        self.branch_filter_job = self.branch_filter_job.wrapping_add(1);
        let job = self.branch_filter_job;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(BRANCH_FILTER_DEBOUNCE_MS))
                .await;
            this.update(cx, |this, cx| {
                if this.branch_filter_job != job {
                    return;
                }
                if this.store.branch_filter_pending() {
                    this.store.commit_branch_filter();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn capture_scroll(&mut self) {
        match self.store.app_page() {
            AppPage::Branches => {
                let y = if self.perf_full {
                    -self.branch_scroll.offset().y.as_f32()
                } else {
                    perf_harness::list_offset(&self.branch_list_scroll)
                };
                self.store.pages.branch_scroll = y.round() as i32;
            }
            AppPage::History => {
                let y = if self.perf_full {
                    -self.history_scroll.offset().y.as_f32()
                } else {
                    perf_harness::list_offset(&self.history_list_scroll)
                };
                self.store.pages.history_scroll = y.round() as i32;
            }
            AppPage::Workspace => {}
        }
    }

    fn restore_scroll(&self) {
        match self.store.app_page() {
            AppPage::Branches => {
                let y = -(self.store.pages.branch_scroll as f32);
                if self.perf_full {
                    self.branch_scroll.set_offset(point(px(0.), px(y)));
                } else {
                    perf_harness::set_list_offset(
                        &self.branch_list_scroll,
                        self.store.pages.branch_scroll,
                    );
                }
            }
            AppPage::History => {
                let y = -(self.store.pages.history_scroll as f32);
                if self.perf_full {
                    self.history_scroll.set_offset(point(px(0.), px(y)));
                } else {
                    perf_harness::set_list_offset(
                        &self.history_list_scroll,
                        self.store.pages.history_scroll,
                    );
                }
            }
            AppPage::Workspace => {}
        }
    }

    fn switch_page(&mut self, page: AppPage, cx: &mut Context<Self>) {
        let before = self.store.app_page();
        self.capture_scroll();
        let effect = self.store.show_page(page);
        if self.store.app_page() != before {
            self.restore_scroll();
            self.shown_page = self.store.app_page();
        }
        self.activate(effect, cx);
        cx.notify();
    }

    fn persist_columns(&self) {
        if let Some(path) = &self.ui_path {
            let _ = store::save_fractions(path, self.store.column_fractions);
        }
    }

    fn place_effect(&mut self, effect: Effect, x: f32, y: f32, cx: &mut Context<Self>) {
        self.store.place_menu(x, y);
        self.activate(effect, cx);
        cx.notify();
    }

    #[cfg(feature = "devtools")]
    fn require_preview(&self) -> Result<(), String> {
        if self.preview {
            Ok(())
        } else {
            Err("This command writes changes and only runs in the preview.".to_string())
        }
    }

    fn scroll_diff(&mut self) {
        if let Some(index) = self.store.take_scroll() {
            self.diff_scroll
                .scroll_to_item_strict(index, ScrollStrategy::Top);
        }
    }

    fn scroll_lists(&mut self) {
        self.scroll_diff();
        if self.store.pane == Pane::Uncommitted {
            let index = self.store.cursor(Pane::Uncommitted);
            self.file_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        if self.store.pane == Pane::Branches {
            let index = self.store.cursor(Pane::Branches);
            self.lane_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        if self.store.pane == Pane::Detail && matches!(self.store.side, SideFocus::Branch { .. }) {
            let index = self.store.cursor(Pane::Detail);
            self.detail_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
    }
}

impl Render for GbWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The previous draw's clock has already stored its sample. A covered
        // window never gets a display link, so each recording draw schedules
        // the next one after this render returns.
        self.settle_recorded_frame(cx);
        let _clock = {
            let recording = self.perf.borrow().recording;
            recording.then(|| RenderClock {
                start: Instant::now(),
                sink: Rc::clone(&self.perf),
            })
        };
        if self.perf.borrow().recording {
            self.kick_perf_draw(cx);
        }
        let theme = ns_ui::theme();
        let viewport = window.viewport_size().width.as_f32();
        self.viewport_width = viewport;
        self.laid_out = store::column_pixels(self.store.column_fractions, viewport);
        let page = self.store.app_page();
        if page != self.shown_page {
            self.restore_scroll();
            self.shown_page = page;
        }
        // Toasts once per frame: the overlay gate and the overlay itself share it.
        let toasts = self.store.activity.visible_toasts();
        let toasts_open = self.store.toast.is_some() || !toasts.is_empty();
        div()
            .id("gb-root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.on_key(&event.keystroke, cx);
            }))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.text)
            .font_family(ns_ui::MONO_FONT)
            .child(self.header(cx))
            .child(self.strip(cx))
            .child(self.body(cx))
            .child(
                StatusBar::new().left(
                    div()
                        .text_color(theme.muted)
                        .child(status_line(&self.store, self.preview)),
                ),
            )
            .when(self.store.project_menu, |root| {
                root.child(self.menu_catcher(cx))
            })
            .when(self.store.project_menu, |root| {
                root.child(self.project_menu(cx))
            })
            .when(self.overlay_open(), |root| {
                root.child(self.overlay_catcher(cx))
            })
            .when(self.store.menu.is_some(), |root| {
                root.child(self.action_menu(window, cx))
            })
            .when(self.store.confirm.is_some(), |root| {
                root.child(self.confirm_popover(cx))
            })
            .when(self.store.remote.integrate_open, |root| {
                root.child(self.integrate_panel(cx))
            })
            .when(self.store.remote.push_menu.is_some(), |root| {
                root.child(self.push_menu_panel(cx))
            })
            .when(
                self.store.remote.push_confirm.is_some() || self.store.remote.integrate_confirm,
                |root| root.child(self.remote_confirm(cx)),
            )
            .when(self.store.settings.open, |root| {
                root.child(self.settings_page(cx))
            })
            .when(
                toasts_open
                    && self.store.confirm.is_none()
                    && self.store.remote.push_confirm.is_none()
                    && !self.store.remote.integrate_confirm
                    && self.store.remote.askpass.is_none(),
                |root| root.child(self.toast_overlay(toasts, cx)),
            )
            .when(self.column_drag.is_some(), |root| {
                root.child(self.column_drag_overlay(cx))
            })
            .when(self.page_catcher_open(), |root| {
                root.child(self.page_catcher(cx))
            })
            .when(self.store.pages.conflicts_open, |root| {
                root.child(self.conflict_panel(cx))
            })
            .when(self.store.pages.ask.is_some(), |root| {
                root.child(self.page_ask(cx))
            })
            .when(self.store.help, |root| root.child(self.help_card(cx)))
            .when(self.store.remote.askpass.is_some(), |root| {
                root.child(self.askpass_modal(cx))
            })
    }
}

impl GbWindow {
    fn close_detail(&mut self) {
        self.store.side = SideFocus::None;
        self.store.commit_loading = false;
        self.store.commit_error = None;
        if self.store.pane == Pane::Detail {
            self.store.pane = Pane::Branches;
        }
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        div()
            .flex_none()
            .h(px(44.))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap_3()
                    .overflow_hidden()
                    .child(div().flex_none().text_sm().child("gb-gpui"))
                    .child(self.project_button(cx))
                    .child(self.mode_chip())
                    .child(self.page_tabs(cx)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.fetch_button(cx))
                    .child(self.fetch_status())
                    .child(self.undo_button(cx))
                    .child(self.settings_button(cx))
                    .child(
                        div()
                            .id("keyboard-tip")
                            .tooltip(Tooltip::text("Show keyboard shortcuts"))
                            .child(ns_ui::quiet_button(
                                "keyboard",
                                "keyboard",
                                "Keyboard",
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.store.toggle_help();
                                    cx.notify();
                                }),
                            )),
                    )
                    .child(
                        div()
                            .id("refresh-tip")
                            .tooltip(Tooltip::text(
                                "Re-read projects, lanes, and uncommitted changes",
                            ))
                            .child(ns_ui::button(
                                "refresh",
                                "refresh",
                                "Refresh",
                                cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx)),
                            )),
                    ),
            )
    }

    fn project_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = project_button_label(&self.store);
        let tip = project_button_tip(&self.store);
        div()
            .id("project-menu-tip")
            .flex_none()
            .max_w(px(240.))
            .overflow_hidden()
            .tooltip(Tooltip::text(tip))
            .child(ns_ui::quiet_button(
                "project-menu",
                "folder",
                label,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.store.toggle_project_menu();
                    cx.notify();
                }),
            ))
    }

    fn mode_chip(&self) -> impl IntoElement {
        let tip = if self.preview {
            "Preview data. Pull requests and suggested messages stay in memory."
        } else {
            "This build has no forge account and no AI provider. Publish stays off until sign-in exists, and Suggest message explains how to set a provider up."
        };
        div()
            .id("m6-chip")
            .flex_none()
            .tooltip(Tooltip::text(tip))
            .child(ns_ui::chip("M6 · settings", Tone::Accent))
    }

    fn settings_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-tip")
            .flex_none()
            .tooltip(Tooltip::text("Open settings (,)"))
            .child(ns_ui::quiet_button(
                "settings",
                "settings",
                "Settings",
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    let effect = this.store.toggle_settings();
                    this.activate(effect, cx);
                    cx.notify();
                }),
            ))
    }

    fn page_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .child(self.page_tab(
                AppPage::Workspace,
                "Workspace",
                "Uncommitted changes, the diff, and branches",
                cx,
            ))
            .child(self.page_tab(
                AppPage::Branches,
                "Branches",
                "Browse local and remote branches (b). Escape returns to the workspace.",
                cx,
            ))
            .child(self.page_tab(
                AppPage::History,
                "History",
                "Snapshot history (h). Escape returns to the workspace.",
                cx,
            ))
    }

    fn page_tab(
        &self,
        page: AppPage,
        label: &'static str,
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let on = self.store.app_page() == page;
        div()
            .id(SharedString::from(format!("page-{}", page.name())))
            .h(px(24.))
            .px_2()
            .flex_none()
            .rounded_md()
            .flex()
            .items_center()
            .text_xs()
            .cursor_pointer()
            .border_1()
            .border_color(if on {
                gpui::Hsla::from(theme.accent)
            } else {
                gpui::Hsla::from(theme.border)
            })
            .when(on, |tab| tab.bg(theme.selected).text_color(theme.accent))
            .when(!on, |tab| tab.text_color(theme.text))
            .tooltip(Tooltip::text(tip))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.switch_page(page, cx);
            }))
            .child(label)
    }

    fn fetch_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let spinning = self.store.activity.spinner_on("fetch");
        let starting = self.store.remote.fetching && !spinning;
        let label = if spinning { "Fetching…" } else { "Fetch" };
        let tip = if spinning {
            pace_sentence("fetch", "Fetching from the remote.")
        } else if starting {
            "Fetch is starting.".to_string()
        } else {
            self.store.remote.fetch_tip()
        };
        div()
            .id("fetch-tip")
            .flex_none()
            .tooltip(Tooltip::text(tip))
            .child(ns_ui::button(
                "fetch",
                "download",
                label,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    if this.store.remote.fetching {
                        return;
                    }
                    let effect = this.store.request_fetch();
                    this.activate(effect, cx);
                    cx.notify();
                }),
            ))
    }

    fn fetch_status(&self) -> impl IntoElement {
        let theme = ns_ui::theme();
        let text = self.store.remote.status_line();
        let warn = self.store.remote.fetch_error.is_some();
        div()
            .id("fetch-status")
            .flex_none()
            .w(px(112.))
            .h(px(20.))
            .px_1()
            .flex()
            .items_center()
            .overflow_hidden()
            .tooltip(Tooltip::text(text.clone()))
            .child(
                div()
                    .min_w(px(0.))
                    .text_xs()
                    .truncate()
                    .text_color(if warn { theme.err } else { theme.muted })
                    .child(text),
            )
    }

    fn undo_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let caption = self.store.undo_caption();
        let locked = self.store.mutation_blocked();
        let enabled = locked.is_none() && caption.is_some() && self.store.busy.is_none();
        let caption = locked
            .map(str::to_string)
            .unwrap_or_else(|| caption.unwrap_or_else(|| "Nothing to undo".to_string()));
        let shown = if enabled {
            truncate_chars(&caption, 28)
        } else {
            "Undo".to_string()
        };
        let theme = ns_ui::theme();
        div()
            .id("undo-tip")
            .flex_none()
            .max_w(px(240.))
            .tooltip(Tooltip::text(caption.clone()))
            .child(if enabled {
                ns_ui::quiet_button(
                    "undo",
                    "restart",
                    shown,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        let effect = this.store.request_undo();
                        this.activate(effect, cx);
                        cx.notify();
                    }),
                )
                .into_any_element()
            } else {
                div()
                    .id("undo-disabled")
                    .px_2()
                    .py(px(4.))
                    .rounded_md()
                    .text_sm()
                    .text_color(theme.muted)
                    .child("Undo")
                    .into_any_element()
            })
    }

    fn strip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.store.pages.editing() {
            return self.edit_strip(cx);
        }
        let theme = ns_ui::theme();
        let text = self.store.strip_text();
        let warn = self.store.banner().is_some() || self.store.exclusive == Some(false);
        let color = if warn { theme.warn } else { theme.muted };
        div()
            .id("status-strip")
            .flex_none()
            .h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .overflow_hidden()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .when(!text.is_empty(), |strip| {
                strip.tooltip(Tooltip::text(text.clone()))
            })
            .child(
                div()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(color)
                    .truncate()
                    .child(text),
            )
            .into_any_element()
    }

    fn body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.store.needs_setup() {
            return div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .child(self.setup_card(cx))
                .into_any_element();
        }
        match self.store.app_page() {
            AppPage::Branches => div()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(self.branches_page(cx))
                .into_any_element(),
            AppPage::History => div()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(self.history_panel(cx))
                .into_any_element(),
            AppPage::Workspace => self.workspace_columns(cx).into_any_element(),
        }
    }

    fn workspace_columns(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .overflow_hidden()
            .child(self.uncommitted_column(cx))
            .child(self.column_handle(0, cx))
            .child(self.changes_column(cx))
            .child(self.column_handle(1, cx))
            .child(self.branches_column(cx))
    }

    fn column(&self, index: usize, active: bool, child: impl IntoElement) -> impl IntoElement {
        let theme = ns_ui::theme();
        let width = self.laid_out.get(index).copied().unwrap_or(0.0);
        div()
            .w(px(width))
            .flex_none()
            .min_w(px(0.))
            .min_h(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.editor)
            .border_1()
            .border_color(if active {
                gpui::Hsla::from(theme.accent)
            } else {
                gpui::Hsla::from(theme.border)
            })
            .child(child)
    }

    fn column_handle(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(SharedString::from(format!("col-handle-{index}")))
            .w(px(store::HANDLE_WIDTH))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .tooltip(Tooltip::text(
                "Drag to resize columns. Double-click to reset.",
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if event.click_count >= 2 {
                        this.store.column_fractions = store::COLUMN_DEFAULT;
                        this.column_drag = None;
                        this.persist_columns();
                        cx.notify();
                        return;
                    }
                    this.column_drag = Some(ColumnDrag {
                        handle: index,
                        origin_x: event.position.x.as_f32(),
                        origin: this.store.column_fractions,
                        viewport: window.viewport_size().width.as_f32(),
                    });
                    cx.notify();
                }),
            )
    }

    fn column_drag_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("column-drag")
            .absolute()
            .inset_0()
            .occlude()
            .cursor_col_resize()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some((handle, origin_x, origin, viewport)) = this
                    .column_drag
                    .as_ref()
                    .map(|drag| (drag.handle, drag.origin_x, drag.origin, drag.viewport))
                else {
                    return;
                };
                let delta = event.position.x.as_f32() - origin_x;
                this.store.column_fractions = store::drag_handle(origin, handle, delta, viewport);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.column_drag = None;
                    this.persist_columns();
                    cx.notify();
                }),
            )
    }

    fn column_header(
        &self,
        id: &'static str,
        title: String,
        tip: String,
        trailing: impl IntoElement,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        div()
            .flex_none()
            .h(px(40.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .w(px(3.))
                    .h(px(14.))
                    .flex_none()
                    .rounded_sm()
                    .bg(theme.accent),
            )
            .child(
                div()
                    .id(id)
                    .flex_1()
                    .min_w(px(0.))
                    .text_sm()
                    .truncate()
                    .tooltip(Tooltip::text(tip))
                    .child(title),
            )
            .child(trailing)
    }

    fn uncommitted_column(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.column(
            0,
            self.store.pane == Pane::Uncommitted,
            self.uncommitted_body(cx),
        )
    }

    fn uncommitted_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let header = self.column_header(
            "uncommitted-title",
            "Uncommitted changes".to_string(),
            "Uncommitted changes".to_string(),
            self.view_toggle(cx),
        );
        let pending = self
            .store
            .activity
            .surface_pending(store::Surface::Uncommitted);
        let busy = self
            .store
            .activity
            .surface_busy(store::Surface::Uncommitted);
        let no_tree = self.store.worktree.is_none();
        let body = if busy {
            div()
                .flex_1()
                .min_h(px(0.))
                .child(ns_ui::loading_pane(
                    "uncommitted-loading",
                    "Reading changes",
                ))
                .into_any_element()
        } else if pending && no_tree {
            quiet_body()
        } else if self
            .store
            .worktree
            .as_ref()
            .is_none_or(|worktree| worktree.files.is_empty())
        {
            div()
                .flex_1()
                .min_h(px(0.))
                .child(wrapped_note(
                    "Nothing to commit. Change a file in the worktree and it will show up here.",
                ))
                .into_any_element()
        } else if self.perf_full {
            let rows = match self.store.view {
                ViewMode::List => self.list_rows(cx),
                ViewMode::Tree => self.tree_rows(cx),
            };
            self.painted
                .set(self.painted.get().saturating_add(rows.len() as u64));
            ScrollArea::new("uncommitted-scroll")
                .vertical()
                .flex_1()
                .min_h(px(0.))
                .child(div().w_full().flex().flex_col().pb_2().children(rows))
                .into_any_element()
        } else {
            self.virtual_files(cx)
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(body)
            .child(self.composer_bar(cx))
            .into_any_element()
    }

    fn view_toggle(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let list_on = self.store.view == ViewMode::List;
        let list = self.view_button(
            "view-list",
            IconName::List,
            "List view",
            list_on,
            ViewMode::List,
            cx,
        );
        let tree = self.view_button(
            "view-tree",
            IconName::Folder,
            "Tree view",
            !list_on,
            ViewMode::Tree,
            cx,
        );
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .child(list)
            .child(tree)
            .into_any_element()
    }

    fn view_button(
        &self,
        id: &'static str,
        icon: IconName,
        tip: &'static str,
        on: bool,
        mode: ViewMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        IconButton::new(id, icon)
            .variant(if on {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Ghost
            })
            .size(ControlSize::Sm)
            .tooltip(tip)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.set_view(mode);
                cx.notify();
            }))
            .into_any_element()
    }

    fn group_heading(
        &self,
        group: &store::ChangeGroup,
        paths: Arc<[String]>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let name = group.name.clone();
        let tip = format!("Actions for {name}");
        div()
            .id(SharedString::from(format!("group-{name}")))
            .px_2()
            .pt_2()
            .pb(px(2.))
            .flex()
            .items_center()
            .gap_2()
            .tooltip(Tooltip::text(tip))
            .on_mouse_down(MouseButton::Right, {
                let paths = Arc::clone(&paths);
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let effect = this.store.open_paths_actions(paths.to_vec());
                    this.place_effect(
                        effect,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        cx,
                    );
                })
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(theme.muted)
                    .truncate()
                    .child(name.clone()),
            )
            .child(more_button(
                SharedString::from(format!("group-more-{name}")),
                cx.listener(move |this, event: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    let effect = this.store.open_paths_actions(paths.to_vec());
                    let position = event.position();
                    this.place_effect(effect, position.x.as_f32(), position.y.as_f32(), cx);
                }),
            ))
    }

    fn virtual_files(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        let painted = Rc::clone(&self.painted);
        match self.store.view {
            ViewMode::List => {
                let slots = self.store.file_slots();
                let count = slots.len();
                uniform_list("uncommitted-rows", count, move |range, _, cx| {
                    let slots = Arc::clone(&slots);
                    painted.set(painted.get().saturating_add(range.len() as u64));
                    entity.update(cx, |this, cx| {
                        let mut out = Vec::new();
                        for index in range {
                            let Some(slot) = slots.get(index) else {
                                continue;
                            };
                            out.push(this.paint_file_slot(slot, cx));
                        }
                        out
                    })
                })
                .track_scroll(&self.file_scroll)
                .size_full()
                .into_any_element()
            }
            ViewMode::Tree => {
                let rows = self.store.tree_rows();
                let count = rows.len();
                uniform_list("uncommitted-tree", count, move |range, _, cx| {
                    let rows = Arc::clone(&rows);
                    painted.set(painted.get().saturating_add(range.len() as u64));
                    entity.update(cx, |this, cx| {
                        let mut out = Vec::new();
                        for index in range {
                            let Some(row) = rows.get(index) else {
                                continue;
                            };
                            out.push(this.tree_row(index, row, cx).into_any_element());
                        }
                        out
                    })
                })
                .track_scroll(&self.file_scroll)
                .size_full()
                .into_any_element()
            }
        }
    }

    fn paint_file_slot(&self, slot: &store::FileSlot, cx: &mut Context<Self>) -> gpui::AnyElement {
        match slot {
            store::FileSlot::Group { name, files, paths } => {
                let group = store::ChangeGroup {
                    name: name.clone(),
                    files: files.clone(),
                };
                self.group_heading(&group, Arc::clone(paths), cx)
                    .into_any_element()
            }
            store::FileSlot::File { flat, index } => {
                let Some(file) = self
                    .store
                    .worktree
                    .as_ref()
                    .and_then(|worktree| worktree.files.get(*index))
                else {
                    return div().into_any_element();
                };
                let label = self.store.lane_label(*index).unwrap_or_default();
                self.file_row(*flat, file, &label, cx).into_any_element()
            }
        }
    }

    fn list_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut flat = 0usize;
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for group in self.store.change_groups() {
            let paths: Arc<[String]> = group
                .files
                .iter()
                .filter_map(|index| {
                    self.store
                        .worktree
                        .as_ref()
                        .and_then(|worktree| worktree.files.get(*index))
                        .map(|file| file.path.clone())
                })
                .collect::<Vec<_>>()
                .into();
            rows.push(self.group_heading(&group, paths, cx).into_any_element());
            for file_index in group.files {
                let Some(file) = self
                    .store
                    .worktree
                    .as_ref()
                    .and_then(|worktree| worktree.files.get(file_index))
                else {
                    flat += 1;
                    continue;
                };
                let label = self.store.lane_label(file_index).unwrap_or_default();
                rows.push(self.file_row(flat, file, &label, cx).into_any_element());
                flat += 1;
            }
        }
        rows
    }

    fn file_row(
        &self,
        flat: usize,
        file: &FileChange,
        label: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let selected = self.store.cursor(Pane::Uncommitted) == flat;
        let focused = self.store.pane == Pane::Uncommitted && selected;
        let path = file.path.clone();
        let label = label.to_string();
        let tip = format!("{path} · {label}. Shift-click selects a range.");
        let marked = self.store.file_marked(&path);
        div()
            .id(SharedString::from(format!("file-{path}")))
            .mx_2()
            .my(px(1.))
            .px_2()
            .min_h(px(28.))
            .rounded_md()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(focused, &theme))
            .when(selected, |row| row.bg(theme.selected))
            .when(!selected, |row| {
                row.hover(move |style| style.bg(theme.hover))
            })
            .tooltip(Tooltip::text(tip))
            .on_mouse_down(MouseButton::Right, {
                let path = path.clone();
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.store.pane = Pane::Uncommitted;
                    this.store.set_cursor(Pane::Uncommitted, flat);
                    let effect = this.store.open_file_actions(&path);
                    this.place_effect(
                        effect,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        cx,
                    );
                })
            })
            .on_click({
                let path = path.clone();
                cx.listener(move |this, event: &ClickEvent, _, cx| {
                    this.store.pane = Pane::Uncommitted;
                    this.store.set_cursor(Pane::Uncommitted, flat);
                    if event.modifiers().shift {
                        this.store.mark_click(&path, true);
                        cx.notify();
                        return;
                    }
                    let effect = this.store.open_current();
                    this.activate(effect, cx);
                    cx.notify();
                })
            })
            .child(inclusion_box(
                SharedString::from(format!("mark-file-{path}")),
                marked,
                format!(
                    "Select {path}. Shift-click selects a range. Space does this from the keyboard."
                ),
                {
                    let path = file.path.clone();
                    cx.listener(move |this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.pane = Pane::Uncommitted;
                        this.store.set_cursor(Pane::Uncommitted, flat);
                        this.store.mark_click(&path, event.modifiers().shift);
                        cx.notify();
                    })
                },
            ))
            .child(status_mark(file.status))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .truncate()
                    .child(file.path.clone()),
            )
            .child(counts(file.additions, file.deletions))
            .child(self.assignment_chip(format!("assign-list-{path}"), &label))
            .when_some(self.store.activity.row_label(&path), |row, tip| {
                row.child(wait_slot(
                    SharedString::from(format!("file-busy-{path}")),
                    tip,
                ))
            })
            .child(more_button(
                SharedString::from(format!("file-more-{path}")),
                cx.listener({
                    let path = path.clone();
                    move |this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.pane = Pane::Uncommitted;
                        this.store.set_cursor(Pane::Uncommitted, flat);
                        let effect = this.store.open_file_actions(&path);
                        let position = event.position();
                        this.place_effect(effect, position.x.as_f32(), position.y.as_f32(), cx);
                    }
                }),
            ))
    }

    fn assignment_chip(&self, id: String, label: &str) -> impl IntoElement {
        let tone = if label == "split" {
            Tone::Warn
        } else if label == "Unassigned" {
            Tone::Neutral
        } else {
            Tone::Accent
        };
        let tip = if label == "split" {
            "Hunks in this file are assigned to more than one lane".to_string()
        } else if label == "Unassigned" {
            "No lane owns the hunks in this file".to_string()
        } else {
            format!("Assigned to {label}")
        };
        div()
            .id(SharedString::from(id))
            .flex_none()
            .tooltip(Tooltip::text(tip))
            .child(ns_ui::chip(label.to_string(), tone))
    }

    fn tree_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        self.store
            .tree_rows()
            .iter()
            .enumerate()
            .map(|(index, row)| self.tree_row(index, row, cx).into_any_element())
            .collect()
    }

    fn tree_row(&self, index: usize, row: &TreeRow, cx: &mut Context<Self>) -> impl IntoElement {
        if row.file.is_none() {
            self.folder_row(index, row, cx).into_any_element()
        } else {
            self.tree_file_row(index, row, cx).into_any_element()
        }
    }

    fn folder_row(&self, index: usize, row: &TreeRow, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let selected = self.store.cursor(Pane::Uncommitted) == index;
        let focused = self.store.pane == Pane::Uncommitted && selected;
        let path = row.path.clone();
        let name = row.name.clone();
        let count = row.file_count;
        let additions = row.additions;
        let deletions = row.deletions;
        let depth = row.depth;
        let mark = if row.expanded { "▾" } else { "▸" };
        let action = if row.expanded { "Collapse" } else { "Expand" };
        let folder_on = self.folder_fully_marked(&path);
        div()
            .id(SharedString::from(format!("tree-{index}")))
            .mx_2()
            .my(px(1.))
            .pl(px(8. + depth as f32 * 12.))
            .pr_2()
            .min_h(px(28.))
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(focused, &theme))
            .when(selected, |line| line.bg(theme.selected))
            .when(!selected, |line| {
                line.hover(move |style| style.bg(theme.hover))
            })
            .tooltip(Tooltip::text(format!("{action} {path} · {count} files")))
            .on_mouse_down(MouseButton::Right, {
                let path = path.clone();
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.store.pane = Pane::Uncommitted;
                    this.store.set_cursor(Pane::Uncommitted, index);
                    let paths = this.paths_in_folder(&path);
                    let effect = this.store.open_paths_actions(paths);
                    this.place_effect(
                        effect,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        cx,
                    );
                })
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.pane = Pane::Uncommitted;
                this.store.set_cursor(Pane::Uncommitted, index);
                let effect = this.store.open_current();
                this.activate(effect, cx);
                cx.notify();
            }))
            .child(inclusion_box(
                SharedString::from(format!("mark-folder-{index}")),
                folder_on,
                format!("Select every file in {path}. Space does this from the keyboard."),
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.store.pane = Pane::Uncommitted;
                    this.store.set_cursor(Pane::Uncommitted, index);
                    let _ = this.store.apply_key("space");
                    cx.notify();
                }),
            ))
            .child(div().flex_none().text_color(theme.muted).child(mark))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_sm()
                    .truncate()
                    .child(name),
            )
            .child(
                div()
                    .id(SharedString::from(format!("folder-count-{index}")))
                    .flex_none()
                    .tooltip(Tooltip::text(format!("{count} files in this folder")))
                    .child(ns_ui::chip(count.to_string(), Tone::Neutral)),
            )
            .child(counts(additions, deletions))
            .when_some(self.store.activity.row_label(&path), |row, tip| {
                row.child(wait_slot(
                    SharedString::from(format!("folder-busy-{index}")),
                    tip,
                ))
            })
            .child(more_button(
                SharedString::from(format!("folder-more-{index}")),
                cx.listener({
                    let path = path.clone();
                    move |this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.pane = Pane::Uncommitted;
                        this.store.set_cursor(Pane::Uncommitted, index);
                        let paths = this.paths_in_folder(&path);
                        let effect = this.store.open_paths_actions(paths);
                        let position = event.position();
                        this.place_effect(effect, position.x.as_f32(), position.y.as_f32(), cx);
                    }
                }),
            ))
    }

    fn tree_file_row(
        &self,
        index: usize,
        row: &TreeRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let selected = self.store.cursor(Pane::Uncommitted) == index;
        let focused = self.store.pane == Pane::Uncommitted && selected;
        let file_index = row.file.unwrap_or(0);
        // Borrow the file: only counts/status are read, never its hunks.
        let file = self
            .store
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.files.get(file_index));
        let path = row.path.clone();
        let name = row.name.clone();
        let depth = row.depth;
        let additions = file.map_or(row.additions, |file| file.additions);
        let deletions = file.map_or(row.deletions, |file| file.deletions);
        let status = file.map_or(FileStatus::Modified, |file| file.status);
        let label = self
            .store
            .lane_label(file_index)
            .unwrap_or_else(|| "Unassigned".to_string());
        let tip = format!("{path} · {label}. Shift-click selects a range.");
        let marked = self.store.file_marked(&path);
        div()
            .id(SharedString::from(format!("tree-{index}")))
            .mx_2()
            .my(px(1.))
            .pl(px(8. + depth as f32 * 12.))
            .pr_2()
            .min_h(px(28.))
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(focused, &theme))
            .when(selected, |line| line.bg(theme.selected))
            .when(!selected, |line| {
                line.hover(move |style| style.bg(theme.hover))
            })
            .tooltip(Tooltip::text(tip))
            .on_mouse_down(MouseButton::Right, {
                let path = path.clone();
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.store.pane = Pane::Uncommitted;
                    this.store.set_cursor(Pane::Uncommitted, index);
                    let effect = this.store.open_file_actions(&path);
                    this.place_effect(
                        effect,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        cx,
                    );
                })
            })
            .on_click({
                let path = path.clone();
                cx.listener(move |this, event: &ClickEvent, _, cx| {
                    this.store.pane = Pane::Uncommitted;
                    this.store.set_cursor(Pane::Uncommitted, index);
                    if event.modifiers().shift {
                        this.store.mark_click(&path, true);
                        cx.notify();
                        return;
                    }
                    let effect = this.store.open_current();
                    this.activate(effect, cx);
                    cx.notify();
                })
            })
            .child(inclusion_box(
                SharedString::from(format!("mark-tree-{index}")),
                marked,
                format!(
                    "Select {path}. Shift-click selects a range. Space does this from the keyboard."
                ),
                {
                    let path = path.clone();
                    cx.listener(move |this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.pane = Pane::Uncommitted;
                        this.store.set_cursor(Pane::Uncommitted, index);
                        this.store.mark_click(&path, event.modifiers().shift);
                        cx.notify();
                    })
                },
            ))
            .child(status_mark(status))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .truncate()
                    .child(name),
            )
            .child(counts(additions, deletions))
            .child(self.assignment_chip(format!("assign-tree-{file_index}"), &label))
            .when_some(self.store.activity.row_label(&path), |row, tip| {
                row.child(wait_slot(
                    SharedString::from(format!("tree-busy-{index}")),
                    tip,
                ))
            })
            .child(more_button(
                SharedString::from(format!("tree-more-{index}")),
                cx.listener({
                    let path = path.clone();
                    move |this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.pane = Pane::Uncommitted;
                        this.store.set_cursor(Pane::Uncommitted, index);
                        let effect = this.store.open_file_actions(&path);
                        let position = event.position();
                        this.place_effect(effect, position.x.as_f32(), position.y.as_f32(), cx);
                    }
                }),
            ))
    }

    fn changes_column(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.column(1, self.store.pane == Pane::Changes, self.changes_body(cx))
    }

    fn changes_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let heading = self.store.changes_heading();
        let title = heading.clone().unwrap_or_else(|| "Changes".to_string());
        let tip = heading.unwrap_or_else(|| "Changes".to_string());
        let header = self.column_header("changes-title", title, tip, div());
        let body = if self.store.activity.surface_busy(store::Surface::Diff) {
            div()
                .flex_1()
                .min_h(px(0.))
                .child(ns_ui::loading_pane("diff-loading", "Reading the diff"))
                .into_any_element()
        } else if self.store.activity.surface_pending(store::Surface::Diff)
            && matches!(self.store.shown, ShownFile::None)
        {
            quiet_body()
        } else if matches!(self.store.shown, ShownFile::None) {
            div()
                .flex_1()
                .min_h(px(0.))
                .w_full()
                .p_3()
                .child(
                    div()
                        .id("changes-empty")
                        .w_full()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .text_sm()
                        .text_color(ns_ui::theme().muted)
                        .tooltip(Tooltip::text(
                            "Nothing open. Pick a file on the left, or a commit on the right, to read its diff.",
                        ))
                        .child(
                            "Nothing open. Pick a file on the left, or a commit on the right, to read its diff.",
                        ),
                )
                .into_any_element()
        } else {
            self.diff_body(cx)
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn diff_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.store.activity.surface_pending(store::Surface::Diff)
            && self.store.diff_rows.is_empty()
        {
            return quiet_body();
        }
        if let Some(message) = &self.store.diff_error
            && self.store.diff_rows.is_empty()
        {
            return self.error_note("diff-error", message, "file_diff", cx);
        }
        if self.store.diff_rows.is_empty() {
            return div()
                .flex_1()
                .min_h(px(0.))
                .child(wrapped_note("No diff text for this file."))
                .into_any_element();
        }
        let rows = std::sync::Arc::clone(&self.store.diff_rows);
        let cursor = self.store.cursor(Pane::Changes);
        let focused = self.store.pane == Pane::Changes;
        let count = rows.len();
        let shown_path = match &self.store.shown {
            ShownFile::Worktree { path } => Some(path.clone()),
            _ => None,
        };
        let file_hunks = match shown_path.as_deref() {
            Some(path) => self.store.file_hunk_headers(path),
            None => Arc::new(Vec::new()),
        };
        let busy_path = shown_path
            .as_ref()
            .is_some_and(|path| self.store.activity.row_label(path).is_some());
        if self.perf_full {
            let hunks = Arc::clone(&file_hunks);
            let _scan: Vec<Option<String>> = rows
                .iter()
                .map(|row| {
                    if row.kind == RowKind::Hunk {
                        row.hunk
                            .and_then(|hunk| hunks.get(hunk).cloned())
                            .or_else(|| Some(row.text.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            self.painted
                .set(self.painted.get().saturating_add(rows.len() as u64));
        }
        let truncated = perf_harness::diff_truncated(&rows);
        let ptr = Arc::as_ptr(&rows) as usize;
        if self.shaped_for.get() != ptr {
            self.shaped.borrow_mut().clear();
            self.shaped_for.set(ptr);
        }
        let shaped = Rc::clone(&self.shaped);
        let painted = Rc::clone(&self.painted);
        let entity = cx.entity();
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                uniform_list("diff-rows", count, move |range, _, cx| {
                    let rows = Arc::clone(&rows);
                    let file_hunks = Arc::clone(&file_hunks);
                    let shown_path = shown_path.clone();
                    let shaped = Rc::clone(&shaped);
                    painted.set(painted.get().saturating_add(range.len() as u64));
                    entity.update(cx, |this, _cx| {
                        let mut out = Vec::new();
                        for index in range {
                            let row = &rows[index];
                            let text = perf_harness::intern_line(&shaped, &row.text);
                            let header = if row.kind == RowKind::Hunk {
                                row.hunk
                                    .and_then(|hunk| file_hunks.get(hunk).cloned())
                                    .or_else(|| Some(row.text.clone()))
                            } else {
                                None
                            };
                            let chrome = match (&shown_path, header) {
                                (Some(path), Some(header)) => {
                                    let marked = this.store.file_marked(path)
                                        || this.store.hunk_marked(path, &header);
                                    Some(HunkChrome {
                                        path: path.clone(),
                                        header,
                                        marked,
                                        busy: busy_path,
                                        entity: entity.clone(),
                                    })
                                }
                                _ => None,
                            };
                            out.push(paint_diff_row(
                                row,
                                index,
                                focused && cursor == index,
                                chrome,
                                text,
                            ));
                        }
                        out
                    })
                })
                .track_scroll(&self.diff_scroll)
                .flex_1()
                .size_full(),
            )
            .when(truncated, |column| {
                column.child(div().flex_none().px_2().py_1().child(word_button(
                    SharedString::from("diff-more"),
                    "Show more",
                    "Show the next part of this diff".to_string(),
                    false,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        let effect = this.store.show_more_diff();
                        this.activate(effect, cx);
                        cx.notify();
                    }),
                )))
            })
            .into_any_element()
    }

    fn branches_column(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = matches!(self.store.pane, Pane::Branches | Pane::Detail);
        self.column(2, active, self.branches_body(cx))
    }

    fn branches_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let header = self.column_header(
            "branches-title",
            "Branches".to_string(),
            "Branches".to_string(),
            self.upstream_chip(cx),
        );
        if !self.store.detail_open() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .child(header)
                .child(self.branches_scroll(cx))
                .into_any_element();
        }
        // Two equal flex children, so the detail pane is half the column.
        // The column header stays in the top half. This split is the only jump.
        let theme = ns_ui::theme();
        div()
            .size_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(header)
                    .child(self.branches_scroll(cx)),
            )
            .child(
                div()
                    .id("branch-detail")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(self.detail_pane(cx)),
            )
            .into_any_element()
    }

    fn branches_scroll(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self
            .store
            .activity
            .surface_busy(store::Surface::BranchesColumn)
        {
            return div()
                .flex_1()
                .min_h(px(0.))
                .child(ns_ui::loading_pane("branches-loading", "Reading branches"))
                .into_any_element();
        }
        if self
            .store
            .activity
            .surface_pending(store::Surface::BranchesColumn)
            && self.store.workspace.is_none()
        {
            return quiet_body().into_any_element();
        }
        let Some(workspace) = self.store.workspace.clone() else {
            let text = if self.store.blocks_open() {
                "Projects stay closed until the installed GitButler matches this build."
            } else if self.store.projects.is_empty() {
                "Add a project in GitButler, then press Refresh."
            } else {
                "Choose a project to see its branches."
            };
            return div()
                .flex_1()
                .min_h(px(0.))
                .child(wrapped_note(text))
                .into_any_element();
        };
        let rows = self.store.lane_rows();
        let target = workspace
            .target_branch
            .clone()
            .unwrap_or_else(|| "No target branch".to_string());
        if !self.perf_full && rows.len() > perf_harness::LANE_CARD_MAX {
            return self.virtual_lanes(target, rows, cx);
        }
        self.painted
            .set(self.painted.get().saturating_add(rows.len() as u64));
        let mut body: Vec<gpui::AnyElement> = Vec::new();
        body.push(target_line(&target).into_any_element());
        if workspace.stacks.is_empty() {
            body.push(
                wrapped_note(
                    "No branches in the workspace yet. Open the Branches page and apply one.",
                )
                .into_any_element(),
            );
        }
        let mut row_index = 0usize;
        for (stack_index, stack) in workspace.stacks.iter().enumerate() {
            body.push(
                self.lane_card(stack_index, stack, &rows, &mut row_index, cx)
                    .into_any_element(),
            );
        }
        ScrollArea::new("branches-scroll")
            .vertical()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .w_full()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(body),
            )
            .into_any_element()
    }

    fn virtual_lanes(
        &self,
        target: String,
        rows: Arc<Vec<store::LaneItem>>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let count = rows.len();
        let entity = cx.entity();
        let painted = Rc::clone(&self.painted);
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(target_line(&target))
            .child(
                uniform_list("lane-rows", count, move |range, _, cx| {
                    let rows = Arc::clone(&rows);
                    painted.set(painted.get().saturating_add(range.len() as u64));
                    entity.update(cx, |this, cx| {
                        let mut out = Vec::new();
                        for index in range {
                            let Some(item) = rows.get(index).copied() else {
                                continue;
                            };
                            out.push(this.paint_lane(index, item, cx));
                        }
                        out
                    })
                })
                .track_scroll(&self.lane_scroll)
                .flex_1()
                .size_full(),
            )
            .into_any_element()
    }

    fn paint_lane(
        &self,
        index: usize,
        item: store::LaneItem,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let looked = self.store.workspace.as_ref().and_then(|workspace| {
            let branch = workspace
                .stacks
                .get(item.stack)?
                .branches
                .get(item.branch)?;
            let commit = item
                .commit
                .and_then(|commit_index| branch.commits.get(commit_index).cloned());
            Some((
                branch.name.clone(),
                branch.tip_sha.clone(),
                branch.tip_full.clone(),
                commit,
            ))
        });
        let inner = match looked {
            None => div().into_any_element(),
            Some((_, _, _, Some(commit))) => self.commit_row(index, &commit, cx).into_any_element(),
            Some((name, tip_sha, tip_full, None)) => {
                let branch = gb_bridge::BranchSummary {
                    name,
                    tip_sha,
                    tip_full,
                    commits: Vec::new(),
                };
                self.branch_header(item.stack, index, item, &branch, cx)
                    .into_any_element()
            }
        };
        // uniform_list spaces every row by the measured height of item 0.
        // Headers and commit rows differ; pin one height so they cannot overlap.
        div()
            .h(px(perf_harness::LANE_ROW_HEIGHT))
            .w_full()
            .overflow_hidden()
            .child(inner)
            .into_any_element()
    }

    fn lane_card(
        &self,
        stack_index: usize,
        stack: &StackSummary,
        rows: &[store::LaneItem],
        row_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        for branch in &stack.branches {
            if *row_index >= rows.len() {
                break;
            }
            let header = rows[*row_index];
            children.push(
                self.branch_header(stack_index, *row_index, header, branch, cx)
                    .into_any_element(),
            );
            *row_index += 1;
            if self
                .store
                .collapsed
                .contains(&collapse_key(stack_index, &branch.name))
            {
                continue;
            }
            for commit in &branch.commits {
                children.push(self.commit_row(*row_index, commit, cx).into_any_element());
                *row_index += 1;
            }
        }
        div()
            .p_2()
            .rounded_md()
            .bg(theme.raised)
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap_1()
            .children(children)
    }

    fn branch_header(
        &self,
        stack_index: usize,
        index: usize,
        item: store::LaneItem,
        branch: &gb_bridge::BranchSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(self.branch_title_row(stack_index, index, item, branch, cx))
            .child(self.branch_review_line(&branch.name, cx))
    }

    fn branch_title_row(
        &self,
        _stack_index: usize,
        index: usize,
        item: store::LaneItem,
        branch: &gb_bridge::BranchSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let selected = self.store.cursor(Pane::Branches) == index;
        let focused = self.store.pane == Pane::Branches && selected;
        let collapsed = self
            .store
            .collapsed
            .contains(&collapse_key(item.stack, &branch.name));
        let mark = if collapsed { "▸" } else { "▾" };
        let action = if collapsed { "Expand" } else { "Collapse" };
        let name = branch.name.clone();
        let tip = branch.tip_sha.clone();
        let short_tip: String = tip.chars().take(7).collect();
        let chevron_tip = format!("{action} {name}");
        let row_tip = if tip.is_empty() {
            format!("Focus branch {name}")
        } else {
            format!("Focus branch {name} · {tip}")
        };
        div()
            .id(SharedString::from(format!("branch-{index}")))
            .px_2()
            .min_h(px(28.))
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(focused, &theme))
            .when(selected, |row| row.bg(theme.selected))
            .tooltip(Tooltip::text(row_tip))
            .on_mouse_down(MouseButton::Right, {
                let name = name.clone();
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.store.set_cursor(Pane::Branches, index);
                    this.open_lane_menu_at(
                        &name,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        cx,
                    );
                })
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.set_cursor(Pane::Branches, index);
                let effect = this.store.open_current();
                this.activate(effect, cx);
                cx.notify();
            }))
            .child(
                div()
                    .id(SharedString::from(format!("chevron-{index}")))
                    .w(px(18.))
                    .flex_none()
                    .cursor_pointer()
                    .text_color(theme.muted)
                    .tooltip(Tooltip::text(chevron_tip))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.toggle_branch_collapsed(index);
                        cx.notify();
                    }))
                    .child(mark),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .font_family(ns_ui::CODE_FONT)
                    .text_sm()
                    .truncate()
                    .child(name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .text_color(theme.muted)
                    .child(if tip.is_empty() {
                        "no tip".to_string()
                    } else {
                        short_tip
                    }),
            )
            .child(self.lane_push_status(&name))
            .child(more_button(
                SharedString::from(format!("lane-more-{index}")),
                cx.listener({
                    let name = name.clone();
                    move |this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.set_cursor(Pane::Branches, index);
                        let position = event.position();
                        this.open_lane_menu_at(&name, position.x.as_f32(), position.y.as_f32(), cx);
                    }
                }),
            ))
    }

    fn open_lane_menu_at(&mut self, name: &str, x: f32, y: f32, cx: &mut Context<Self>) {
        match self.store.open_lane_actions(name) {
            Ok(()) => self.place_effect(Effect::None, x, y, cx),
            Err(error) => {
                self.store.toast_bad = true;
                self.store.toast = Some(error);
                cx.notify();
            }
        }
    }

    fn lane_push_status(&self, lane: &str) -> impl IntoElement {
        let theme = ns_ui::theme();
        let busy = self.store.remote.push_busy.as_deref() == Some(lane);
        let result = self.store.remote.push_results.get(lane).cloned();
        let pushing = busy
            && self.store.activity.spinner_on("push")
            && self.store.activity.row("push").as_deref() == Some(lane);
        if pushing {
            return div()
                .id(SharedString::from(format!("push-busy-{lane}")))
                .w(px(112.))
                .h(px(16.))
                .flex_none()
                .overflow_hidden()
                .flex()
                .items_center()
                .text_xs()
                .text_color(theme.accent)
                .tooltip(Tooltip::text(pace_sentence("push", "Pushing this lane.")))
                .child("Pushing…")
                .into_any_element();
        }
        if busy {
            return div()
                .id(SharedString::from(format!("push-start-{lane}")))
                .w(px(112.))
                .h(px(16.))
                .flex_none()
                .tooltip(Tooltip::text("Push is starting."))
                .into_any_element();
        }
        let text = result.clone().unwrap_or_default();
        let warn = text.to_ascii_lowercase().contains("rejected")
            || text.to_ascii_lowercase().contains("did not");
        div()
            .id(SharedString::from(format!("push-status-{lane}")))
            .w(px(112.))
            .h(px(16.))
            .flex_none()
            .overflow_hidden()
            .flex()
            .items_center()
            .text_xs()
            .text_color(if warn { theme.err } else { theme.muted })
            .when(result.is_some(), |row| {
                row.tooltip(Tooltip::text(text.clone()))
            })
            .child(div().min_w(px(0.)).truncate().child(text))
            .into_any_element()
    }

    fn branch_review_line(&self, branch: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let name = branch.to_string();
        if let Some(review) = self.store.review_for(&name).cloned() {
            return self.pr_chip(&name, &review, cx);
        }
        if self.store.forge.publishing.as_deref() == Some(name.as_str()) {
            return wait_slot(
                SharedString::from(format!("publish-busy-{name}")),
                format!("Opening a draft pull request for {name}"),
            );
        }
        div().into_any_element()
    }

    fn pr_chip(
        &self,
        branch: &str,
        review: &gb_bridge::Review,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let dot = match review.ci {
            gb_bridge::CiState::Passing => theme.ok,
            gb_bridge::CiState::Failing => theme.err,
            gb_bridge::CiState::Pending => theme.warn,
            gb_bridge::CiState::None => theme.muted,
        };
        let tip = format!(
            "Pull request #{} for {branch} is {}. {}. {}",
            review.number,
            review.state.label(),
            review.ci.label(),
            review.title
        );
        let lane = branch.to_string();
        div()
            .id(SharedString::from(format!("pr-chip-{branch}")))
            .h(px(20.))
            .px_1()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .rounded_sm()
            .border_1()
            .border_color(gpui::Hsla::from(theme.border))
            .cursor_pointer()
            .tooltip(Tooltip::text(tip))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                let effect = this.store.show_review(&lane);
                this.activate(effect, cx);
                cx.notify();
            }))
            .child(
                div()
                    .w(px(8.))
                    .h(px(8.))
                    .flex_none()
                    .rounded(px(4.))
                    .bg(dot),
            )
            .child(
                div()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .child(format!("#{}", review.number)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(review.state.label()),
            )
            .into_any_element()
    }

    fn commit_row(
        &self,
        index: usize,
        commit: &CommitSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let selected = self.store.cursor(Pane::Branches) == index;
        let focused = self.store.pane == Pane::Branches && selected;
        let sha = commit.sha.clone();
        let short = commit.short_sha.clone();
        let title = commit.title.clone();
        let when = relative(commit.authored_at);
        let author = commit.author.clone();
        let conflicted = commit.conflicted;
        let pushed = commit.pushed;
        div()
            .id(SharedString::from(format!("commit-{index}")))
            .px_2()
            .py_1()
            .rounded_md()
            .flex()
            .flex_col()
            .gap(px(2.))
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(focused, &theme))
            .when(selected, |row| row.bg(theme.selected))
            .tooltip(Tooltip::text(format!("Focus commit {sha}: {title}")))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.set_cursor(Pane::Branches, index);
                let effect = this.store.open_current();
                this.activate(effect, cx);
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Right, {
                let sha = sha.clone();
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_row_actions(
                        index,
                        &sha,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        cx,
                    );
                })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .font_family(ns_ui::CODE_FONT)
                            .text_xs()
                            .text_color(theme.accent)
                            .child(short),
                    )
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .flex_1()
                            .min_w(px(0.))
                            .child(title),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(div().flex_1().min_w(px(0.)).truncate().child(author))
                    .child(div().flex_none().child(when))
                    .when_some(self.store.activity.row_label(&sha), |row, tip| {
                        row.child(wait_slot(
                            SharedString::from(format!("commit-busy-{index}")),
                            tip,
                        ))
                    })
                    .child(more_button(
                        SharedString::from(format!("commit-more-{index}")),
                        cx.listener({
                            let sha = sha.clone();
                            move |this, event: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                let position = event.position();
                                this.open_row_actions(
                                    index,
                                    &sha,
                                    position.x.as_f32(),
                                    position.y.as_f32(),
                                    cx,
                                );
                            }
                        }),
                    ))
                    .when(conflicted, |row| {
                        row.child(marker_chip(
                            index,
                            "conflict",
                            "This commit has conflicts. Resolve them from More actions.",
                            Tone::Err,
                        ))
                    })
                    .when(pushed, |row| {
                        row.child(marker_chip(
                            index,
                            "pushed",
                            "This commit is on the remote",
                            Tone::Ok,
                        ))
                    }),
            )
    }

    fn detail_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.store.side {
            SideFocus::Branch { .. } => self.branch_detail(cx).into_any_element(),
            SideFocus::Commit { sha } => {
                let sha = sha.clone();
                self.commit_detail(&sha, cx).into_any_element()
            }
            SideFocus::None => div().into_any_element(),
        }
    }

    fn detail_header(
        &self,
        title: String,
        tip: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        div()
            .flex_none()
            .h(px(32.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("detail-title")
                    .flex_1()
                    .min_w(px(0.))
                    .text_sm()
                    .truncate()
                    .tooltip(Tooltip::text(tip))
                    .child(title),
            )
            .child(
                IconButton::new("detail-close", IconName::X)
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Sm)
                    .tooltip("Close the detail")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.close_detail();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn commit_detail(&self, sha: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self
            .store
            .commit
            .as_ref()
            .is_some_and(|commit| commit.sha == sha || commit.short_sha == sha);
        let title = format!("Commit {}", short7(sha));
        let header = self.detail_header(title.clone(), title, cx);
        let body = if self.store.activity.spinner_on("commit_details") && !ready {
            div()
                .flex_1()
                .min_h(px(0.))
                .child(ns_ui::loading_pane("commit-loading", "Reading the commit"))
                .into_any_element()
        } else if self.store.activity.is_loading("commit_details") && !ready {
            quiet_body()
        } else if let Some(message) = &self.store.commit_error
            && !ready
        {
            self.error_note("commit-error", message, "commit_details", cx)
        } else if let Some(details) = self
            .store
            .commit
            .clone()
            .filter(|commit| commit.sha == sha || commit.short_sha == sha)
        {
            self.commit_facts(&details, cx).into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(px(0.))
                .child(wrapped_note("Reading the commit."))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(self.reword_bar(sha))
            .child(self.detail_note())
            .child(body)
    }

    fn commit_facts(&self, details: &CommitDetails, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let additions: u32 = details.files.iter().map(|file| file.additions).sum();
        let deletions: u32 = details.files.iter().map(|file| file.deletions).sum();
        let totals = format!("+{additions} -{deletions} in {} files", details.files.len());
        let parents_short = if details.parents.is_empty() {
            "none".to_string()
        } else {
            details
                .parents
                .iter()
                .map(|sha| short7(sha))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let sha = details.sha.clone();
        let mut files: Vec<gpui::AnyElement> = Vec::new();
        for (index, file) in details.files.iter().enumerate() {
            files.push(self.commit_file_row(index, file, cx).into_any_element());
        }
        ScrollArea::new("commit-scroll")
            .vertical()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .id("commit-message")
                            .w_full()
                            .min_w(px(0.))
                            .whitespace_normal()
                            .text_sm()
                            .tooltip(Tooltip::text(details.message.clone()))
                            .child(compact_message(&details.message)),
                    )
                    .child(meta_line("meta-author", "Author", &details.author))
                    .child(meta_line("meta-committer", "Committer", &details.committer))
                    .child(meta_line(
                        "meta-authored",
                        "Authored",
                        &dated(details.authored_at),
                    ))
                    .child(meta_line(
                        "meta-committed",
                        "Committed",
                        &dated(details.committed_at),
                    ))
                    .child(meta_line("meta-parents", "Parents", &parents_short))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .overflow_hidden()
                            .child(
                                div()
                                    .id("commit-sha")
                                    .flex_1()
                                    .min_w(px(0.))
                                    .font_family(ns_ui::CODE_FONT)
                                    .text_xs()
                                    .text_color(theme.muted)
                                    .truncate()
                                    .tooltip(Tooltip::text(sha.clone()))
                                    .child(sha.clone()),
                            )
                            .child(copy_button("copy-sha", sha, cx)),
                    )
                    .child(
                        div()
                            .id("commit-totals")
                            .text_xs()
                            .font_family(ns_ui::CODE_FONT)
                            .tooltip(Tooltip::text(totals.clone()))
                            .child(totals),
                    )
                    .child(
                        div()
                            .pt_1()
                            .text_xs()
                            .text_color(theme.muted)
                            .child("Files"),
                    )
                    .children(files),
            )
    }

    fn commit_file_row(
        &self,
        index: usize,
        file: &CommitFile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let selected = self.store.cursor(Pane::Detail) == index
            && matches!(self.store.side, SideFocus::Commit { .. });
        let focused = self.store.pane == Pane::Detail && selected;
        let path = file.path.clone();
        div()
            .id(SharedString::from(format!("detail-file-{index}")))
            .px_2()
            .min_h(px(28.))
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(focused, &theme))
            .when(selected, |row| row.bg(theme.selected))
            .tooltip(Tooltip::text(format!("Show the diff for {path}")))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.set_cursor(Pane::Detail, index);
                let effect = this.store.open_current();
                this.activate(effect, cx);
                cx.notify();
            }))
            .child(status_mark(file.status))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .truncate()
                    .child(path),
            )
            .child(counts(file.additions, file.deletions))
    }

    fn branch_detail(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(info) = self.store.branch_info() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.detail_header("Branch".to_string(), "Branch".to_string(), cx))
                .child(wrapped_note("This branch is no longer in the workspace."))
                .into_any_element();
        };
        let title = info.name.clone();
        let header = self.detail_header(title.clone(), format!("Branch {title}"), cx);
        let facts = self.branch_facts(&info, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(self.detail_note())
            .child(facts)
            .into_any_element()
    }

    fn branch_facts(&self, info: &BranchFocusInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let full = if info.tip_full.is_empty() {
            info.tip_sha.clone()
        } else {
            info.tip_full.clone()
        };
        let shown = if full.is_empty() {
            "no tip".to_string()
        } else {
            full.clone()
        };
        let authors = if info.authors.is_empty() {
            None
        } else {
            Some(info.authors.join(", "))
        };
        let dates = date_range(info.earliest, info.latest);
        // The commit list is virtualized: only visible rows are built, so a
        // 3,000-commit lane costs a viewport of element trees, not all of them.
        let commits = Arc::clone(&info.commits);
        let count = commits.len();
        let has_commits = count > 0;
        let entity = cx.entity();
        let painted = Rc::clone(&self.painted);
        div()
            .w_full()
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .overflow_hidden()
            .child(meta_line("branch-stack", "Stack", &info.stack_label))
            .child(meta_line(
                "branch-count",
                "Commits",
                &info.commit_count.to_string(),
            ))
            .when_some(info.push_state.clone(), |column, state| {
                column.child(self.push_chip(&state))
            })
            .when_some(authors, |column, authors| {
                column.child(meta_line("branch-authors", "Authors", &authors))
            })
            .when_some(dates, |column, dates| {
                column.child(meta_line_wrap("branch-dates", "Dates", &dates))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(72.))
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted)
                            .child("Tip"),
                    )
                    .child(
                        div()
                            .id("branch-tip")
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(ns_ui::CODE_FONT)
                            .text_xs()
                            .truncate()
                            .tooltip(Tooltip::text(shown.clone()))
                            .child(shown.clone()),
                    )
                    .child(copy_button("copy-sha", full, cx)),
            )
            .child(
                div()
                    .pt_1()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted)
                    .child("Commits"),
            )
            .when(info.commits.is_empty(), |column| {
                column.child(wrapped_note("No commits on this branch yet."))
            })
            .when(has_commits, |column| {
                column.child(
                    uniform_list("branch-detail-rows", count, move |range, _, cx| {
                        let commits = Arc::clone(&commits);
                        painted.set(painted.get().saturating_add(range.len() as u64));
                        entity.update(cx, |this, cx| {
                            let mut out = Vec::new();
                            for index in range {
                                let Some(commit) = commits.get(index) else {
                                    continue;
                                };
                                // The gap_1 spacing the old flex column gave rows.
                                out.push(
                                    div()
                                        .pb(px(4.))
                                        .child(this.branch_commit_row(index, commit, cx)),
                                );
                            }
                            out
                        })
                    })
                    .track_scroll(&self.detail_scroll)
                    .flex_1()
                    .min_h(px(0.)),
                )
            })
    }

    fn push_chip(&self, state: &str) -> impl IntoElement {
        let tone = match state {
            "Pushed" => Tone::Ok,
            "Partly pushed" => Tone::Warn,
            _ => Tone::Neutral,
        };
        let tip = match state {
            "Pushed" => "Every commit on this branch is on the remote",
            "Partly pushed" => "Some commits on this branch are on the remote",
            "Not pushed" => "No commit on this branch is on the remote",
            _ => "Push state from the commits on this branch",
        };
        div()
            .id("push-state")
            .self_start()
            .tooltip(Tooltip::text(tip))
            .child(ns_ui::chip(state.to_string(), tone))
    }

    fn branch_commit_row(
        &self,
        index: usize,
        commit: &store::CommitLine,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let selected = self.store.cursor(Pane::Detail) == index
            && matches!(self.store.side, SideFocus::Branch { .. });
        let focused = self.store.pane == Pane::Detail && selected;
        let short = commit.short_sha.clone();
        let title = commit.title.clone();
        div()
            .id(SharedString::from(format!("detail-commit-{index}")))
            .px_2()
            .min_h(px(28.))
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(focused, &theme))
            .when(selected, |row| row.bg(theme.selected))
            .tooltip(Tooltip::text(format!("Focus commit {short}: {title}")))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.set_cursor(Pane::Detail, index);
                let effect = this.store.open_current();
                this.activate(effect, cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .text_color(theme.accent)
                    .child(short),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_sm()
                    .truncate()
                    .child(title),
            )
    }

    fn menu_catcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("menu-catcher")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.store.set_project_menu(false);
                    cx.notify();
                }),
            )
    }

    fn project_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if self.store.projects.is_empty() {
            rows.push(wrapped_note(self.menu_empty_text()).into_any_element());
        } else {
            for (index, project) in self.store.projects.iter().enumerate() {
                rows.push(self.project_menu_row(index, project, cx).into_any_element());
            }
        }
        div()
            .id("project-menu-panel")
            .absolute()
            .top(px(42.))
            .left(px(72.))
            .w(px(340.))
            .max_h(px(360.))
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                ScrollArea::new("project-menu-scroll")
                    .vertical()
                    .max_h(px(280.))
                    .child(div().w_full().p_1().flex().flex_col().children(rows)),
            )
            .child(self.project_add(cx))
    }

    fn project_add(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let marked = self.store.project_cursor() == self.store.projects.len();
        let open = self.store.remote.add_open;
        let focused = self.store.remote.add_focused;
        let path = self.store.remote.add_path.clone();
        let shown = if path.is_empty() && !focused {
            "Folder path".to_string()
        } else if focused {
            format!("{path}▌")
        } else {
            path.clone()
        };
        div()
            .flex_none()
            .w_full()
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("add-project")
                    .w_full()
                    .px_2()
                    .min_h(px(28.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .border_1()
                    .border_color(ring(marked, &theme))
                    .when(marked, |row| row.bg(theme.selected))
                    .text_sm()
                    .tooltip(Tooltip::text(
                        "Remember a repository that is already on this computer. Type its folder path.",
                    ))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.open_add_field();
                        cx.notify();
                    }))
                    .child("Add local repository…"),
            )
            .when(open, |column| {
                column
                    .child(
                        div()
                            .id("add-project-path")
                            .w_full()
                            .h(px(28.))
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(if focused {
                                gpui::Hsla::from(theme.accent)
                            } else {
                                gpui::Hsla::from(theme.border)
                            })
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .font_family(ns_ui::CODE_FONT)
                            .text_xs()
                            .text_color(if path.is_empty() && !focused {
                                theme.muted
                            } else {
                                theme.text
                            })
                            .tooltip(Tooltip::text(
                                "Type the folder path, then press Enter. No file dialog is used.",
                            ))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.store.remote.add_focused = true;
                                cx.notify();
                            }))
                            .child(shown),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(word_button(
                                SharedString::from("add-project-go"),
                                "Add",
                                "Remember this folder as a project".to_string(),
                                false,
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    let path = this.store.remote.add_path.clone();
                                    let effect = this.store.request_add(path);
                                    this.activate(effect, cx);
                                    cx.notify();
                                }),
                            ))
                            .child(word_button(
                                SharedString::from("add-project-cancel"),
                                "Cancel",
                                "Close the path field".to_string(),
                                false,
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.store.remote.add_open = false;
                                    this.store.remote.add_focused = false;
                                    cx.notify();
                                }),
                            )),
                    )
            })
    }

    fn menu_empty_text(&self) -> String {
        if let Some(banner) = self.store.banner() {
            return banner;
        }
        if let Some(reason) = &self.store.list_reason {
            return reason.clone();
        }
        if self.store.blocks_open() {
            return "Projects stay closed until the installed GitButler matches this build."
                .to_string();
        }
        "No projects yet. Add a project in GitButler, then press Refresh.".to_string()
    }

    fn project_menu_row(
        &self,
        index: usize,
        project: &gb_bridge::ProjectSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ns_ui::theme();
        let id = project.id.clone();
        let title = project.title.clone();
        let path = project.path.clone();
        let hint = self.store.project_hint(&id);
        let selected = self.store.selected.as_deref() == Some(id.as_str());
        let marked = self.store.project_cursor() == index;
        let mut tip = format!("{title} · {path}");
        if let Some(hint) = &hint {
            tip.push_str(" · ");
            tip.push_str(hint);
        }
        div()
            .id(SharedString::from(format!("menu-project-{id}")))
            .w_full()
            .px_2()
            .py_1()
            .rounded_md()
            .flex()
            .flex_col()
            .gap(px(2.))
            .cursor_pointer()
            .border_1()
            .border_color(ring(marked, &theme))
            .when(selected || marked, |row| row.bg(theme.selected))
            .when(!(selected || marked), |row| {
                row.hover(move |style| style.bg(theme.hover))
            })
            .tooltip(Tooltip::text(tip))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let effect = this.store.choose_project_at(index);
                this.activate(effect, cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .truncate()
                            .child(title),
                    )
                    .when(selected, |row| {
                        row.child(div().flex_none().text_color(theme.accent).child("✓"))
                    }),
            )
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(theme.muted)
                    .font_family(ns_ui::CODE_FONT)
                    .truncate()
                    .child(path),
            )
            .when_some(hint, |row, hint| {
                row.child(
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .text_xs()
                        .text_color(theme.warn)
                        .whitespace_normal()
                        .child(hint),
                )
            })
    }

    fn setup_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let branches = self.store.remote.branches.clone();
        let selected = self.store.remote.branch_index;
        let push_remote = self.store.remote.push_remote.clone();
        let field_focused = self.store.remote.push_remote_focused;
        let error = self.store.remote.setup_error.clone().unwrap_or_default();
        let busy = self.store.remote.setup_busy;
        let shown = if push_remote.is_empty() && !field_focused {
            "origin".to_string()
        } else if field_focused {
            format!("{push_remote}▌")
        } else {
            push_remote.clone()
        };
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if branches.is_empty() {
            let waiting = if self.store.remote.branches_loaded {
                "This remote has no branches."
            } else {
                "Reading remote branches…"
            };
            rows.push(wrapped_note(waiting).into_any_element());
        }
        for (index, branch) in branches.iter().enumerate() {
            let full = branch.full_name.clone();
            let remote = branch.remote.clone();
            let label = format!("{}/{}", branch.remote, branch.branch);
            let marked = index == selected;
            rows.push(
                div()
                    .id(SharedString::from(format!("setup-branch-{index}")))
                    .w_full()
                    .min_h(px(32.))
                    .px_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .cursor_pointer()
                    .border_1()
                    .border_color(ring(marked, &theme))
                    .when(marked, |row| row.bg(theme.selected))
                    .tooltip(Tooltip::text(format!(
                        "Base your work on {label}. Ref {full}."
                    )))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.store.remote.branch_index = index;
                        this.store.remote.push_remote_focused = false;
                        if !this.store.remote.push_remote_edited {
                            this.store.remote.push_remote = remote.clone();
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .truncate()
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(180.))
                            .font_family(ns_ui::CODE_FONT)
                            .text_xs()
                            .text_color(theme.muted)
                            .truncate()
                            .child(full),
                    )
                    .into_any_element(),
            );
        }
        div()
            .id("setup-card")
            .w(px(460.))
            .max_w_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .shadow(cx.theme().elevation(Elevation::Floating))
            .child(div().text_sm().child("Choose the branch your work is based on"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted)
                    .whitespace_normal()
                    .child("Your commits sit on top of this branch. Pick the remote branch, then the remote you push to."),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(rows),
            )
            .child(div().text_xs().text_color(theme.muted).child("Push remote"))
            .child(
                div()
                    .id("setup-push-remote")
                    .w_full()
                    .h(px(28.))
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if field_focused {
                        gpui::Hsla::from(theme.accent)
                    } else {
                        gpui::Hsla::from(theme.border)
                    })
                    .bg(theme.editor)
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .cursor_pointer()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .tooltip(Tooltip::text(
                        "The remote that receives pushes. Enter saves the target branch.",
                    ))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.remote.push_remote_focused = true;
                        cx.notify();
                    }))
                    .child(shown),
            )
            .child(if busy {
                wait_slot(
                    SharedString::from("setup-busy"),
                    "Saving the target branch".to_string(),
                )
            } else {
                word_button(
                    SharedString::from("set-target"),
                    "Set target",
                    "Use the selected branch as the base for this project".to_string(),
                    false,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        let effect = this.store.request_selected_target();
                        this.activate(effect, cx);
                        cx.notify();
                    }),
                )
            })
            .child(
                div()
                    .id("setup-error")
                    .h(px(18.))
                    .w_full()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(theme.err)
                    .when(!error.is_empty(), |row| row.tooltip(Tooltip::text(error.clone())))
                    .child(div().min_w(px(0.)).truncate().child(error)),
            )
    }

    fn upstream_chip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(label) = self.store.remote.upstream_chip() else {
            return div().into_any_element();
        };
        let locked = self.store.mutation_blocked().map(str::to_string);
        let tip = locked
            .clone()
            .unwrap_or_else(|| format!("{label}. Review them and update your lanes."));
        div()
            .id("upstream-chip")
            .flex_none()
            .when(locked.is_none(), |chip| chip.cursor_pointer())
            .tooltip(Tooltip::text(tip))
            .when(locked.is_none(), |chip| {
                chip.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    let effect = this.store.open_integrate();
                    this.activate(effect, cx);
                    cx.notify();
                }))
            })
            .child(ns_ui::chip(label, Tone::Warn))
            .into_any_element()
    }

    fn integrate_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let commits = self.store.remote.upstream.clone();
        let lanes = self.store.lane_names();
        let cursor = self.store.remote.integrate_cursor;
        let error = self
            .store
            .remote
            .integrate_error
            .clone()
            .unwrap_or_default();
        let summary = self
            .store
            .remote
            .preview
            .as_ref()
            .map(|preview| preview.summary.clone())
            .unwrap_or_else(|| {
                if self.store.remote.preview_busy {
                    "Checking each lane…".to_string()
                } else {
                    "Not checked yet".to_string()
                }
            });
        let mut commit_rows: Vec<gpui::AnyElement> = Vec::new();
        for (index, commit) in commits.iter().enumerate() {
            let line = format!("{}  {}", commit.short_sha, commit.title);
            commit_rows.push(
                div()
                    .id(SharedString::from(format!("upstream-commit-{index}")))
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .tooltip(Tooltip::text(format!(
                        "{} {} by {}",
                        commit.short_sha, commit.title, commit.author
                    )))
                    .child(div().min_w(px(0.)).truncate().child(line))
                    .into_any_element(),
            );
        }
        let mut lane_rows: Vec<gpui::AnyElement> = Vec::new();
        for (index, lane) in lanes.iter().enumerate() {
            let strategy = match self.store.remote.strategy_of(lane) {
                gb_bridge::UpstreamStrategy::Rebase => "Rebase",
                gb_bridge::UpstreamStrategy::Merge => "Merge",
            };
            let outcome = self.store.remote.lane_outcome(lane);
            let tone = match outcome {
                "Conflicts" => Tone::Err,
                "Clean" => Tone::Ok,
                _ => Tone::Neutral,
            };
            let marked = cursor == index;
            let busy = self.store.remote.integrate_busy.contains(lane);
            let lane_name = lane.clone();
            lane_rows.push(
                div()
                    .id(SharedString::from(format!("integrate-lane-{index}")))
                    .min_h(px(32.))
                    .px_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .border_1()
                    .border_color(ring(marked, &theme))
                    .when(marked, |row| row.bg(theme.selected))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(ns_ui::CODE_FONT)
                            .text_sm()
                            .truncate()
                            .child(lane_name.clone()),
                    )
                    .child(if busy {
                        wait_slot(
                            SharedString::from(format!("integrate-busy-{index}")),
                            format!("Updating {lane_name}"),
                        )
                    } else {
                        word_button(
                            SharedString::from(format!("integrate-strategy-{index}")),
                            strategy,
                            format!(
                                "{strategy} {lane_name} onto the upstream. GitButler applies the same choice to every lane."
                            ),
                            false,
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.store.remote.integrate_cursor = index;
                                let effect = this.store.apply_key("enter");
                                this.activate(effect, cx);
                                cx.notify();
                            }),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("integrate-outcome-{index}")))
                            .flex_none()
                            .tooltip(Tooltip::text(format!("{lane_name}: {outcome}")))
                            .child(ns_ui::chip(outcome.to_string(), tone)),
                    )
                    .into_any_element(),
            );
        }
        let apply_marked = cursor == lanes.len();
        div()
            .id("integrate-panel")
            .absolute()
            .top(px(84.))
            .right(px(12.))
            .w(px(400.))
            .max_h(px(560.))
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(32.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .id("integrate-title")
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .truncate()
                            .tooltip(Tooltip::text(
                                "New commits on the branch your work is based on",
                            ))
                            .child(
                                self.store
                                    .remote
                                    .upstream_chip()
                                    .unwrap_or_else(|| "Upstream".to_string()),
                            ),
                    )
                    .child(
                        IconButton::new("integrate-close", IconName::X)
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .tooltip("Close upstream commits")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.store.remote.integrate_open = false;
                                this.store.remote.integrate_confirm = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                ScrollArea::new("integrate-scroll")
                    .vertical()
                    .flex_1()
                    .child(
                        div()
                            .w_full()
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted)
                                    .child("Upstream commits"),
                            )
                            .children(commit_rows)
                            .child(
                                div()
                                    .id("integrate-summary")
                                    .text_xs()
                                    .text_color(theme.muted)
                                    .tooltip(Tooltip::text(summary.clone()))
                                    .child(summary),
                            )
                            .children(lane_rows)
                            .child(
                                div()
                                    .id("integrate-apply-row")
                                    .min_h(px(32.))
                                    .px_1()
                                    .rounded_md()
                                    .flex()
                                    .items_center()
                                    .border_1()
                                    .border_color(ring(apply_marked, &theme))
                                    .when(apply_marked, |row| row.bg(theme.selected))
                                    .child(word_button(
                                        SharedString::from("integrate-apply"),
                                        "Apply",
                                        "Update every lane. You will be asked to confirm."
                                            .to_string(),
                                        false,
                                        cx.listener(|this, _: &ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            this.store.ask_integrate();
                                            cx.notify();
                                        }),
                                    )),
                            )
                            .child(
                                div()
                                    .id("integrate-error")
                                    .h(px(32.))
                                    .w_full()
                                    .overflow_hidden()
                                    .text_xs()
                                    .text_color(theme.err)
                                    .whitespace_normal()
                                    .when(!error.is_empty(), |row| {
                                        row.tooltip(Tooltip::text(error.clone()))
                                    })
                                    .child(error),
                            ),
                    ),
            )
    }

    fn push_menu_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(lane) = self.store.remote.push_menu.clone() else {
            return div().into_any_element();
        };
        let theme = ns_ui::theme();
        let cursor = self.store.remote.push_cursor;
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (index, (label, force)) in store::PUSH_CHOICES.iter().enumerate() {
            let marked = index == cursor;
            let tip = store::push_choice_tip(*force, &lane);
            let danger = *force != ForcePush::No;
            rows.push(
                div()
                    .id(SharedString::from(format!("push-choice-{index}")))
                    .w_full()
                    .min_h(px(28.))
                    .px_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .border_1()
                    .border_color(ring(marked, &theme))
                    .when(marked, |row| row.bg(theme.selected))
                    .text_sm()
                    .text_color(if danger { theme.err } else { theme.text })
                    .tooltip(Tooltip::text(tip))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.store.remote.push_cursor = index;
                        let effect = this.store.apply_key("enter");
                        this.activate(effect, cx);
                        cx.notify();
                    }))
                    .child(*label)
                    .into_any_element(),
            );
        }
        div()
            .id("push-menu")
            .absolute()
            // Below the window header, the column header, and the target line,
            // so the menu does not cover "Target refs/remotes/…".
            .top(px(168.))
            .right(px(16.))
            .w(px(280.))
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("push-menu-title")
                    .px_2()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted)
                    .truncate()
                    .tooltip(Tooltip::text(format!(
                        "Push lane {lane} to origin. Arrows move. Enter chooses. Escape closes."
                    )))
                    .child(format!("Push {lane}")),
            )
            .child(div().w_full().p_1().flex().flex_col().children(rows))
            .into_any_element()
    }

    fn remote_confirm(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let (title, message, confirm, destructive) =
            if let Some(ask) = self.store.remote.push_confirm.clone() {
                (ask.title, ask.message, "Force push", true)
            } else if self.store.remote.integrate_confirm {
                (
                    "Apply upstream commits?".to_string(),
                    self.store.integrate_message(),
                    "Apply",
                    false,
                )
            } else {
                return div().into_any_element();
            };
        let ask = ns_ui::Ask {
            title: title.into(),
            message: message.into(),
            confirm: confirm.into(),
            confirm_icon: "alert",
            destructive,
        };
        div()
            .id("remote-confirm")
            .absolute()
            .bottom(self.overlay_lift())
            .right(px(12.))
            .w(px(420.))
            .rounded_md()
            .border_1()
            .border_color(if destructive { theme.err } else { theme.border })
            .occlude()
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(ns_ui::ask_banner(
                &ask,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    let effect = this.store.apply_key("enter");
                    this.activate(effect, cx);
                    cx.notify();
                }),
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    let _ = this.store.apply_key("escape");
                    cx.notify();
                }),
            ))
            .into_any_element()
    }

    fn askpass_modal(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(request) = self.store.remote.askpass.clone() else {
            return div().into_any_element();
        };
        let theme = ns_ui::theme();
        let label = store::askpass_label(&request);
        let reveal = self.store.remote.reveal;
        let shown = self.store.remote.shown_secret();
        let field = if shown.is_empty() {
            "▌".to_string()
        } else {
            format!("{shown}▌")
        };
        let reveal_tip = if reveal {
            "Hide what you typed"
        } else {
            "Show what you typed"
        };
        div()
            .id("askpass-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., 0.72))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("askpass-dialog")
                    .w(px(420.))
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_md()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.border)
                    .shadow(cx.theme().elevation(Elevation::Floating))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_sm().child("Git needs a sign-in"))
                    .child(
                        div()
                            .id("askpass-prompt")
                            .text_xs()
                            .text_color(theme.muted)
                            .whitespace_normal()
                            .tooltip(Tooltip::text(request.prompt.clone()))
                            .child(request.prompt.clone()),
                    )
                    .child(div().text_xs().text_color(theme.muted).child(label))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id("askpass-field")
                                    .flex_1()
                                    .min_w(px(0.))
                                    .h(px(32.))
                                    .px_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(gpui::Hsla::from(theme.accent))
                                    .bg(theme.editor)
                                    .flex()
                                    .items_center()
                                    .overflow_hidden()
                                    .font_family(ns_ui::CODE_FONT)
                                    .text_sm()
                                    .tooltip(Tooltip::text(format!(
                                        "Type the {label}. Enter submits. Escape cancels."
                                    )))
                                    .child(div().min_w(px(0.)).truncate().child(field)),
                            )
                            .child(
                                IconButton::new(
                                    "askpass-reveal",
                                    if reveal {
                                        IconName::EyeOff
                                    } else {
                                        IconName::Eye
                                    },
                                )
                                .variant(ButtonVariant::Ghost)
                                .size(ControlSize::Sm)
                                .tooltip(reveal_tip)
                                .on_click(cx.listener(
                                    |this, _: &ClickEvent, _, cx| {
                                        this.store.remote.reveal = !this.store.remote.reveal;
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(word_button(
                                SharedString::from("askpass-submit"),
                                "Submit",
                                format!("Send the {label}. It is not saved in the window."),
                                false,
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    let effect = this.store.submit_askpass();
                                    this.activate(effect, cx);
                                    cx.notify();
                                }),
                            ))
                            .when(
                                store::operation_spec("answer_askpass")
                                    .is_some_and(|item| item.cancel)
                                    || store::operation_spec("askpass_pending")
                                        .is_some_and(|item| item.cancel),
                                |row| {
                                    row.child(word_button(
                                        SharedString::from("askpass-cancel"),
                                        "Cancel",
                                        "Dismiss this prompt without sending a value".to_string(),
                                        false,
                                        cx.listener(|this, _: &ClickEvent, _, cx| {
                                            let effect = this.store.cancel_askpass();
                                            this.activate(effect, cx);
                                            cx.notify();
                                        }),
                                    ))
                                },
                            ),
                    ),
            )
            .into_any_element()
    }

    fn composer_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if !self.store.composer.open {
            return self.composer_collapsed(cx);
        }
        let theme = ns_ui::theme();
        let new_lane = self.store.composer.new_lane;
        div()
            .id("composer")
            .flex_none()
            .h(px(188.))
            .w_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(self.composer_lane_row(cx))
            .when(new_lane, |column| {
                column.child(self.composer_field(
                    Field::LaneName,
                    "composer-lane-name",
                    "Lane name",
                    cx,
                ))
            })
            .child(self.composer_field(Field::Title, "composer-title", "Commit title", cx))
            .child(self.composer_ruler())
            .child(self.composer_field(Field::Body, "composer-body", "Commit body", cx))
            .child(self.composer_status())
            .child(self.composer_actions(cx))
            .into_any_element()
    }

    fn composer_collapsed(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let summary = self.store.commit_summary();
        div()
            .id("composer-collapsed")
            .flex_none()
            .h(px(36.))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(if let Some(reason) = self.store.mutation_blocked() {
                inert_word(
                    SharedString::from("composer-open"),
                    "Commit",
                    reason.to_string(),
                )
            } else {
                word_button(
                    SharedString::from("composer-open"),
                    "Commit",
                    "Open the commit editor (c)".to_string(),
                    false,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.set_composer_open(true);
                        cx.notify();
                    }),
                )
            })
            .child(
                div()
                    .id("composer-summary")
                    .flex_1()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(theme.muted)
                    .truncate()
                    .tooltip(Tooltip::text(summary.clone()))
                    .child(summary),
            )
            .into_any_element()
    }

    fn composer_lane_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let label = self.store.composer_lane_label();
        let tip = format!("Commit to {label}. Choose a lane, or New lane.");
        div()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id("composer-lane")
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(24.))
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.editor)
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .cursor_pointer()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .tooltip(Tooltip::text(tip))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.set_project_menu(false);
                        this.store.open_lane_menu();
                        cx.notify();
                    }))
                    .child(div().min_w(px(0.)).truncate().child(label)),
            )
            .child(
                IconButton::new("composer-close", IconName::X)
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Sm)
                    .tooltip("Close the commit editor (c)")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.set_composer_open(false);
                        cx.notify();
                    })),
            )
    }

    fn composer_field(
        &self,
        field: Field,
        id: &'static str,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let value = match field {
            Field::Title => self.store.composer.title.clone(),
            Field::Body => self.store.composer.body.clone(),
            Field::LaneName => self.store.composer.lane_name.clone(),
        };
        let warn = match field {
            Field::Title => self.store.title_over(),
            Field::Body => self.store.body_over(),
            Field::LaneName => false,
        };
        let height = match field {
            Field::Body if self.store.composer.new_lane => px(24.),
            Field::Body => px(48.),
            Field::Title | Field::LaneName => px(24.),
        };
        let focused = self.store.composer.field == Some(field);
        let empty = value.is_empty();
        let shown = if empty && !focused {
            placeholder.to_string()
        } else if focused {
            format!("{value}▌")
        } else {
            value.to_string()
        };
        let color = if empty && !focused {
            theme.muted
        } else if warn {
            theme.warn
        } else {
            theme.text
        };
        let tip = match field {
            Field::Title => "Commit title. Aim for 50 columns.",
            Field::Body => "Commit body. Aim for 72 columns. Enter adds a line.",
            Field::LaneName => "New lane name, with no spaces. Enter leaves the field.",
        };
        div()
            .id(id)
            .h(height)
            .w_full()
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(ring(focused, &theme))
            .bg(theme.editor)
            .overflow_hidden()
            .font_family(ns_ui::CODE_FONT)
            .text_xs()
            .text_color(color)
            .when(field == Field::Body, |field| field.whitespace_normal())
            .cursor_pointer()
            .tooltip(Tooltip::text(tip))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.focus_field(field);
                cx.notify();
            }))
            .child(shown)
            .into_any_element()
    }

    fn composer_ruler(&self) -> impl IntoElement {
        let theme = ns_ui::theme();
        let hint = self.store.ruler_hint();
        let warn = self.store.title_over() || self.store.body_over();
        div()
            .id("composer-ruler")
            .h(px(14.))
            .text_xs()
            .text_color(if warn { theme.warn } else { theme.muted })
            .truncate()
            .tooltip(Tooltip::text(
                "Keep the title within 50 columns and each body line within 72.",
            ))
            .child(hint)
    }

    fn composer_status(&self) -> impl IntoElement {
        let theme = ns_ui::theme();
        let (summary, color) = if let Some(error) = &self.store.composer.suggest_error {
            (error.clone(), theme.err)
        } else if self.store.message_hold.is_some() {
            (store::REPLACE_PROMPT.to_string(), theme.warn)
        } else if let Some(hook) = &self.store.composer.hook {
            let color = if self.store.mutation_error.is_some() {
                theme.err
            } else {
                theme.muted
            };
            (hook.clone(), color)
        } else {
            let color = if self.store.mutation_error.is_some() {
                theme.err
            } else {
                theme.muted
            };
            (self.store.commit_summary(), color)
        };
        div()
            .id("composer-status")
            .h(px(16.))
            .text_xs()
            .text_color(color)
            .truncate()
            .tooltip(Tooltip::text(summary.clone()))
            .child(summary)
    }

    fn composer_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let block = self.store.commit_block();
        let busy = self
            .store
            .busy
            .as_ref()
            .is_some_and(|busy| busy.op == "commit" || busy.op == "lane");
        let spinning = self.store.activity.spinner_on("commit_create")
            || self.store.activity.spinner_on("create_lane");
        let label = if spinning { "Committing…" } else { "Commit" };
        let tip = if spinning {
            pace_sentence("commit_create", "Committing.")
        } else if busy {
            "Starting the commit…".to_string()
        } else {
            block
                .clone()
                .unwrap_or_else(|| "Commit the selected changes to this lane".to_string())
        };
        let commit = if block.is_some() {
            div()
                .id("do-commit-disabled")
                .h(px(24.))
                .px_2()
                .rounded_md()
                .flex()
                .items_center()
                .bg(theme.raised)
                .text_xs()
                .text_color(theme.muted)
                .tooltip(Tooltip::text(tip))
                .child(label)
                .into_any_element()
        } else {
            div()
                .id("do-commit")
                .h(px(24.))
                .px_2()
                .rounded_md()
                .flex()
                .items_center()
                .cursor_pointer()
                .bg(theme.accent)
                .text_xs()
                .text_color(theme.accent_ink)
                .tooltip(Tooltip::text(tip))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    let effect = this.store.plan_commit();
                    this.activate(effect, cx);
                    cx.notify();
                }))
                .child(label)
                .into_any_element()
        };
        div()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_2()
            .child(commit)
            .child(if let Some(reason) = self.store.mutation_blocked() {
                inert_word(
                    SharedString::from("composer-amend"),
                    "Amend into…",
                    reason.to_string(),
                )
            } else {
                word_button(
                    SharedString::from("composer-amend"),
                    "Amend into…",
                    "Amend the selection into an existing commit".to_string(),
                    false,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.set_project_menu(false);
                        this.store.open_amend_menu();
                        cx.notify();
                    }),
                )
            })
            .child(self.suggest_control(cx))
    }

    fn suggest_control(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.store.message_hold.is_some() {
            return div()
                .flex()
                .items_center()
                .gap_1()
                .child(word_button(
                    SharedString::from("suggest-replace"),
                    "Replace message",
                    "Replace the title and body with the suggestion".to_string(),
                    false,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.replace_held_message();
                        cx.notify();
                    }),
                ))
                .child(word_button(
                    SharedString::from("suggest-keep"),
                    "Keep mine",
                    "Keep the title and body you already typed".to_string(),
                    false,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.keep_typed_message();
                        cx.notify();
                    }),
                ))
                .into_any_element();
        }
        if self.store.suggest_busy {
            let spinning = self.store.activity.spinner_on("suggest_message");
            return inert_word(
                SharedString::from("suggest-busy"),
                if spinning {
                    "Suggesting…"
                } else {
                    "Suggest message"
                },
                if spinning {
                    pace_sentence("suggest_message", "Writing a suggestion.")
                } else {
                    "Starting the suggestion…".to_string()
                },
            );
        }
        word_button(
            SharedString::from("suggest-message"),
            "Suggest message",
            "Fill the title and body from the selected file names. Text you already typed stays until you confirm."
                .to_string(),
            false,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let effect = this.store.request_suggestion();
                this.activate(effect, cx);
                cx.notify();
            }),
        )
    }

    fn paths_in_folder(&self, folder: &str) -> Vec<String> {
        self.store.folder_paths_vec(folder)
    }

    fn folder_fully_marked(&self, folder: &str) -> bool {
        self.store.folder_fully_marked(folder)
    }

    fn open_row_actions(
        &mut self,
        index: usize,
        sha: &str,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.store.set_project_menu(false);
        self.store.pane = Pane::Branches;
        self.store.set_cursor(Pane::Branches, index);
        match self.store.open_actions_for(sha) {
            Ok(()) => self.place_effect(Effect::None, x, y, cx),
            Err(error) => {
                self.store.toast_bad = true;
                self.store.toast = Some(error);
                cx.notify();
            }
        }
    }

    fn reword_bar(&self, sha: &str) -> gpui::AnyElement {
        if let Some(draft) = &self.store.reword
            && same_commit(&draft.sha, sha)
        {
            return self.reword_editor(&draft.draft);
        }
        div().into_any_element()
    }

    fn reword_editor(&self, draft: &str) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let shown = if draft.is_empty() {
            "Commit message▌".to_string()
        } else {
            format!("{draft}▌")
        };
        div()
            .id("reword-field")
            .flex_none()
            .h(px(56.))
            .px_2()
            .py(px(2.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .id("reword-draft")
                    .flex_1()
                    .min_h(px(0.))
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(gpui::Hsla::from(theme.accent))
                    .overflow_hidden()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .whitespace_normal()
                    .tooltip(Tooltip::text(
                        "Edit the commit message. Enter saves. Escape cancels.",
                    ))
                    .child(shown),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted)
                    .child("Enter saves. Escape cancels."),
            )
            .into_any_element()
    }

    fn detail_note(&self) -> impl IntoElement {
        let theme = ns_ui::theme();
        let text = self.store.mutation_error.clone().unwrap_or_default();
        div()
            .id("detail-error")
            .flex_none()
            .h(px(18.))
            .px_2()
            .flex()
            .items_center()
            .overflow_hidden()
            .text_xs()
            .text_color(if text.is_empty() {
                theme.muted
            } else {
                theme.err
            })
            .when(!text.is_empty(), |row| {
                row.tooltip(Tooltip::text(text.clone()))
            })
            .child(div().min_w(px(0.)).truncate().child(text))
    }

    fn overlay_open(&self) -> bool {
        self.store.menu.is_some()
            || self.store.confirm.is_some()
            || self.store.remote.integrate_open
            || self.store.remote.push_menu.is_some()
            || self.store.remote.push_confirm.is_some()
            || self.store.remote.integrate_confirm
    }

    fn overlay_catcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("action-catcher")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.store.close_menu();
                    this.store.confirm_no();
                    this.store.remote.close_overlays();
                    cx.notify();
                }),
            )
    }

    /// How far a bottom overlay sits above the window edge so it clears the composer.
    fn overlay_lift(&self) -> gpui::Pixels {
        if self.store.composer.open {
            px(232.)
        } else {
            px(84.)
        }
    }

    fn action_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let title = self.store.menu_title();
        let entries = self.store.menu_entries();
        let cursor = self.store.menu_cursor();
        let naming = self.store.menu_naming();
        let draft = self.store.menu_draft();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let label = entry.label.clone();
            let tip = entry.tip.clone();
            let enabled = entry.enabled;
            let destructive = entry.destructive;
            let marked = index == cursor;
            rows.push(
                div()
                    .id(SharedString::from(format!("menu-entry-{index}")))
                    .w_full()
                    .min_h(px(28.))
                    .px_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if destructive {
                        gpui::Hsla::from(theme.err)
                    } else {
                        ring(marked, &theme)
                    })
                    .when(marked, |row| row.bg(theme.selected))
                    .when(!marked, |row| row.hover(move |style| style.bg(theme.hover)))
                    .text_sm()
                    .text_color(if !enabled {
                        theme.muted
                    } else if destructive {
                        theme.err
                    } else {
                        theme.text
                    })
                    .tooltip(Tooltip::text(tip))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        if this.store.menu_naming()
                            && let Some(menu) = &mut this.store.menu
                        {
                            menu.naming = false;
                        }
                        this.store.set_menu_cursor(index);
                        let effect = this.store.choose_menu();
                        this.activate(effect, cx);
                        cx.notify();
                    }))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(label))
                    .into_any_element(),
            );
        }
        let mut panel = div()
            .id("action-menu")
            .absolute()
            .w(px(280.))
            .max_h(px(360.))
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        // `top` is required: an absolute menu with only `bottom` collapses to nothing.
        let rows_h = entries.len() as f32 * 28.0 + 28.0 + if naming { 36.0 } else { 0.0 };
        let height = rows_h.min(360.0);
        let (anchor_x, anchor_y) = self.store.menu_anchor();
        let size = window.viewport_size();
        let (left, top) = store::clamp_popup(
            anchor_x,
            anchor_y,
            280.0,
            height,
            size.width.as_f32(),
            size.height.as_f32(),
            8.0,
        );
        panel = panel.top(px(top)).left(px(left));
        panel
            .child(
                div()
                    .id("action-menu-title")
                    .px_2()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted)
                    .truncate()
                    .tooltip(Tooltip::text(format!(
                        "{title}. Arrows move. Enter chooses. Escape closes."
                    )))
                    .child(title),
            )
            .child(
                ScrollArea::new("action-menu-scroll")
                    .vertical()
                    .max_h(px(280.))
                    .child(div().w_full().p_1().flex().flex_col().children(rows)),
            )
            .when(naming, |panel| {
                panel.child(
                    div()
                        .id("lane-name-draft")
                        .mx_2()
                        .mb_2()
                        .h(px(28.))
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(gpui::Hsla::from(theme.accent))
                        .flex()
                        .items_center()
                        .font_family(ns_ui::CODE_FONT)
                        .text_xs()
                        .tooltip(Tooltip::text(
                            "Type a lane name with no spaces, then press Enter",
                        ))
                        .child(if draft.is_empty() {
                            "Lane name▌".to_string()
                        } else {
                            format!("{draft}▌")
                        }),
                )
            })
    }

    fn confirm_popover(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(confirm) = self.store.confirm.clone() else {
            return div().into_any_element();
        };
        let theme = ns_ui::theme();
        let ask = ns_ui::Ask {
            title: confirm.title.into(),
            message: confirm.message.into(),
            confirm: confirm.confirm_label.into(),
            confirm_icon: "trash",
            destructive: true,
        };
        div()
            .id("confirm-popover")
            .absolute()
            .bottom(self.overlay_lift())
            .left(px(12.))
            .w(px(420.))
            .rounded_md()
            .border_1()
            .border_color(theme.err)
            .occlude()
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(ns_ui::ask_banner(
                &ask,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    let effect = this.store.confirm_yes();
                    this.activate(effect, cx);
                    cx.notify();
                }),
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.store.confirm_no();
                    cx.notify();
                }),
            ))
            .into_any_element()
    }

    fn toast_overlay(
        &self,
        views: Vec<store::ToastView>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if !views.is_empty() {
            let mut stack = div()
                .id("mutation-toast")
                .absolute()
                .bottom(px(44.))
                .right(px(12.))
                .w(px(360.))
                .flex()
                .flex_col()
                .gap_2()
                .occlude();
            for (index, toast) in views.into_iter().take(3).enumerate() {
                stack = stack.child(self.one_toast(index, toast, cx));
            }
            return stack.into_any_element();
        }
        if self
            .store
            .activity
            .hides_legacy(self.store.toast.as_deref())
        {
            return div().into_any_element();
        }
        let Some(text) = self.store.toast.clone() else {
            return div().into_any_element();
        };
        let theme = ns_ui::theme();
        div()
            .id("mutation-toast")
            .absolute()
            .bottom(px(44.))
            .right(px(12.))
            .w(px(320.))
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(if self.store.toast_bad {
                theme.err
            } else {
                theme.ok
            })
            .occlude()
            .tooltip(Tooltip::text(text.clone()))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(if self.store.toast_bad {
                        theme.err
                    } else {
                        theme.ok
                    })
                    .whitespace_normal()
                    .child(text),
            )
            .into_any_element()
    }

    fn one_toast(
        &self,
        index: usize,
        toast: store::ToastView,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let op = toast.op.clone();
        let details = if toast.details.is_empty() {
            toast.message.clone()
        } else {
            format!("{}: {}", toast.op, toast.details)
        };
        let retry_button = toast.retry.then(|| {
            let op = op.clone();
            word_button(
                SharedString::from(format!("toast-retry-{index}")),
                "Retry",
                "Run this again".to_string(),
                false,
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let effect = this.store.retry_effect(&op);
                    this.activate(effect, cx);
                    cx.notify();
                }),
            )
        });
        let copy_button = toast.bad.then(|| {
            let details = details.clone();
            word_button(
                SharedString::from(format!("toast-copy-{index}")),
                "Copy details",
                "Copy the error details".to_string(),
                false,
                cx.listener(move |_this, _: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(details.clone()));
                    cx.notify();
                }),
            )
        });
        div()
            .id(SharedString::from(format!("toast-{index}")))
            .w_full()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(if toast.bad { theme.err } else { theme.ok })
            .occlude()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .id(SharedString::from(format!("toast-text-{index}")))
                    .w_full()
                    .min_w(px(0.))
                    .text_xs()
                    .whitespace_normal()
                    .text_color(if toast.bad { theme.err } else { theme.ok })
                    .tooltip(Tooltip::text(toast.message.clone()))
                    .child(toast.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(retry_button)
                    .children(copy_button),
            )
            .into_any_element()
    }

    fn error_note(
        &self,
        id: &str,
        message: &str,
        op: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let retry = self.store.activity.can_retry(op);
        let op_id = op.to_string();
        let stored = self.store.activity.details(op);
        let details = if stored == op {
            format!("{op}: {message}")
        } else {
            stored
        };
        let retry_button = retry.then(|| {
            let op_id = op_id.clone();
            word_button(
                SharedString::from(format!("{id}-retry")),
                "Retry",
                "Run this again".to_string(),
                false,
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let effect = this.store.retry_effect(&op_id);
                    this.activate(effect, cx);
                    cx.notify();
                }),
            )
        });
        let copy = details.clone();
        let copy_button = word_button(
            SharedString::from(format!("{id}-copy")),
            "Copy details",
            "Copy the error details".to_string(),
            false,
            cx.listener(move |_this, _: &ClickEvent, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                cx.notify();
            }),
        );
        div()
            .id(SharedString::from(id.to_string()))
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .flex()
            .flex_col()
            .items_start()
            .gap_2()
            .child(
                div()
                    .id(SharedString::from(format!("{id}-text")))
                    .w_full()
                    .min_w(px(0.))
                    .text_sm()
                    .whitespace_normal()
                    .text_color(theme.err)
                    .tooltip(Tooltip::text(message.to_string()))
                    .child(message.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(retry_button)
                    .child(copy_button),
            )
            .into_any_element()
    }

    fn edit_strip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let short = self
            .store
            .pages
            .edit_short
            .clone()
            .unwrap_or_else(|| "commit".to_string());
        let lead = match self.store.pages.phase {
            EditPhase::Entering => "Opening commit",
            EditPhase::Saving => "Saving commit",
            EditPhase::Aborting => "Aborting the edit of",
            EditPhase::Editing | EditPhase::Idle => "Editing commit",
        };
        let ready = self.store.edit_actions_ready();
        let tip = self.store.pages.edit_error.clone().unwrap_or_else(|| {
            let status = self.store.edit_strip_text();
            if status.is_empty() {
                format!("{lead} {short}. Save and return, or abort to leave the commit.")
            } else {
                format!("{status}. Save and return, or abort to leave the commit.")
            }
        });
        let wait = "Wait for this edit to finish.".to_string();
        div()
            .id("status-strip")
            .flex_none()
            .h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .tooltip(Tooltip::text(tip))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text)
                    .child(lead),
            )
            .child(
                div()
                    .id("edit-sha")
                    .flex_none()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .text_color(theme.accent)
                    .child(short),
            )
            .child({
                let err = self.store.pages.edit_error.clone();
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(16.))
                    .text_xs()
                    .truncate()
                    .text_color(if err.is_some() { theme.err } else { theme.muted })
                    .child(err.unwrap_or_else(|| "—".to_string()))
            })
            .child(if ready {
                word_button(
                    SharedString::from("edit-save"),
                    "Save and return",
                    "Save the edit and return to the workspace".to_string(),
                    false,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        match this.store.request_save_edit() {
                            Ok(effect) => this.activate(effect, cx),
                            Err(error) => {
                                this.store.toast_bad = true;
                                this.store.toast = Some(error);
                            }
                        }
                        cx.notify();
                    }),
                )
            } else {
                inert_word(
                    SharedString::from("edit-save"),
                    "Save and return",
                    wait.clone(),
                )
            })
            .child(if ready {
                word_button(
                    SharedString::from("edit-abort"),
                    "Abort",
                    "Leave the edit and return to the workspace. Unsaved edits in the checkout are kept until this succeeds.".to_string(),
                    true,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        match this.store.request_abort_edit() {
                            Ok(effect) => this.activate(effect, cx),
                            Err(error) => {
                                this.store.toast_bad = true;
                                this.store.toast = Some(error);
                            }
                        }
                        cx.notify();
                    }),
                )
            } else {
                inert_word(SharedString::from("edit-abort"), "Abort", wait)
            })
            .into_any_element()
    }

    fn page_catcher_open(&self) -> bool {
        self.store.pages.conflicts_open
    }

    fn page_catcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("page-catcher")
            .absolute()
            .top(px(72.))
            .left(px(0.))
            .right(px(0.))
            .bottom(px(0.))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.store.close_conflicts();
                    cx.notify();
                }),
            )
    }

    fn branches_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let tab = self.store.pages.tab;
        let virtual_branches = self.virtual_branches();
        // The pull-request tab shows reviews, so the branch rows stay cached.
        let rows: Arc<Vec<gb_bridge::BranchRow>> =
            if tab == BranchTab::PullRequests || virtual_branches {
                Arc::new(Vec::new())
            } else {
                self.store.visible_branches()
            };
        let cursor = self.store.pages.branch_cursor;
        let filter = self.store.pages.filter.clone();
        let notice = self.store.pages.branch_notice.clone().unwrap_or_default();
        let notice_bad = self.store.pages.branch_notice_bad;
        let loading = self.store.pages.branches_loading;
        let error = self.store.pages.branches_error.clone();
        let mut list: Vec<gpui::AnyElement> = Vec::new();
        if tab != BranchTab::PullRequests {
            if self
                .store
                .activity
                .surface_busy(store::Surface::BranchesPage)
                || (loading && rows.is_empty() && self.store.activity.spinner_on("list_branches"))
            {
                list.push(
                    ns_ui::loading_pane("branches-page-loading", "Loading branches")
                        .into_any_element(),
                );
            } else if loading && rows.is_empty() {
                list.push(quiet_body());
            } else if let Some(message) = &error {
                list.push(self.error_note("branches-error", message, "list_branches", cx));
            } else if virtual_branches {
            } else if rows.is_empty() {
                let empty = if filter.is_empty() {
                    "No branches on this tab. Switch to Remote, or create a lane from a commit."
                } else {
                    "No branch matches that filter."
                };
                list.push(
                    div()
                        .px_3()
                        .py_3()
                        .text_sm()
                        .text_color(theme.muted)
                        .child(empty)
                        .into_any_element(),
                );
            } else {
                for (index, row) in rows.iter().enumerate() {
                    list.push(self.branch_page_row(index, row, index == cursor, cx));
                }
            }
        }
        let filter_placeholder = if tab == BranchTab::PullRequests {
            "Filter by number, title, or branch"
        } else {
            "Filter by name, commit, or author"
        };
        let filter_tip = if tab == BranchTab::PullRequests {
            "Filter pull requests by number, title, branch, or check state. Backspace deletes."
        } else {
            "Filter branches by name, last commit, or author. Backspace deletes."
        };
        let filter_shown = if filter.is_empty() {
            filter_placeholder.to_string()
        } else {
            format!("{filter}▌")
        };
        div()
            .id("branches-page")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.background)
            .child(
                div()
                    .flex_none()
                    .h(px(40.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(div().text_sm().child("Branches"))
                    .child(self.branch_tab_button(BranchTab::Local, cx))
                    .child(self.branch_tab_button(BranchTab::Remote, cx))
                    .child(self.branch_tab_button(BranchTab::PullRequests, cx))
                    .child(div().flex_1()),
            )
            .child(
                div()
                    .id("branch-filter")
                    .flex_none()
                    .h(px(32.))
                    .mx_3()
                    .mt_2()
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(gpui::Hsla::from(theme.border))
                    .bg(theme.editor)
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .text_color(if filter.is_empty() {
                        theme.muted
                    } else {
                        theme.text
                    })
                    .tooltip(Tooltip::text(filter_tip))
                    .child(filter_shown),
            )
            .child(notice_line("branch-notice", &notice, notice_bad))
            .child(if tab == BranchTab::PullRequests {
                self.pr_review_rows(&filter, cx)
            } else if virtual_branches {
                self.virtual_branch_rows(cursor, cx)
            } else {
                if self.perf_full {
                    self.painted
                        .set(self.painted.get().saturating_add(list.len() as u64));
                }
                ScrollArea::new("branches-page-scroll")
                    .vertical()
                    .track_scroll(&self.branch_scroll)
                    .flex_1()
                    .min_h(px(0.))
                    .child(div().w_full().py_1().flex().flex_col().children(list))
                    .into_any_element()
            })
    }

    fn branch_tab_button(&self, tab: BranchTab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let on = self.store.pages.tab == tab;
        let tip = match tab {
            BranchTab::Local => "Branches on this computer, including ones in the workspace",
            BranchTab::Remote => "Remote-tracking branches",
            BranchTab::PullRequests => {
                if self.store.forge.connected {
                    "Pull requests for this project's branches"
                } else {
                    gb_bridge::FORGE_DISCONNECTED
                }
            }
        };
        div()
            .id(SharedString::from(format!("branch-tab-{}", tab.name())))
            .h(px(24.))
            .px_2()
            .flex_none()
            .rounded_md()
            .flex()
            .items_center()
            .text_xs()
            .cursor_pointer()
            .border_1()
            .border_color(if on {
                gpui::Hsla::from(theme.accent)
            } else {
                gpui::Hsla::from(theme.border)
            })
            .when(on, |button| {
                button.bg(theme.selected).text_color(theme.accent)
            })
            .when(!on, |button| button.text_color(theme.text))
            .tooltip(Tooltip::text(tip))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.set_branch_tab(tab);
                cx.notify();
            }))
            .child(tab.label())
            .into_any_element()
    }

    /// The pull-request tab body. Rows come from the cached filtered reviews
    /// and are virtualized, so a 1,000-review list paints a viewport.
    fn pr_review_rows(&self, filter: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        if !self.store.forge.connected {
            let message = if self.store.forge.detail.is_empty() {
                gb_bridge::FORGE_DISCONNECTED.to_string()
            } else {
                self.store.forge.detail.clone()
            };
            return div()
                .id("pr-empty")
                .px_3()
                .py_3()
                .text_sm()
                .text_color(theme.muted)
                .tooltip(Tooltip::text(message.clone()))
                .child(message)
                .into_any_element();
        }
        let reviews = self.store.visible_reviews();
        if reviews.is_empty() {
            let empty = if filter.is_empty() {
                "This project has no pull requests yet."
            } else {
                "No pull request matches that filter."
            };
            return div()
                .id("pr-empty")
                .px_3()
                .py_3()
                .text_sm()
                .text_color(theme.muted)
                .child(empty)
                .into_any_element();
        }
        let count = reviews.len();
        let cursor = self.store.forge.cursor;
        let entity = cx.entity();
        let painted = Rc::clone(&self.painted);
        uniform_list("pr-rows", count, move |range, _, cx| {
            let reviews = Arc::clone(&reviews);
            painted.set(painted.get().saturating_add(range.len() as u64));
            entity.update(cx, |this, cx| {
                let mut out = Vec::new();
                for index in range {
                    let Some(review) = reviews.get(index) else {
                        continue;
                    };
                    out.push(this.review_page_row(index, review, index == cursor, cx));
                }
                out
            })
        })
        .flex_1()
        .min_h(px(0.))
        .into_any_element()
    }

    fn review_page_row(
        &self,
        index: usize,
        review: &gb_bridge::Review,
        marked: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let number = review.number;
        let state = review.state.label();
        let checks = review.ci.label();
        let branch = review.branch.clone();
        let title = review.title.clone();
        let dot = match review.ci {
            gb_bridge::CiState::Passing => theme.ok,
            gb_bridge::CiState::Failing => theme.err,
            gb_bridge::CiState::Pending => theme.warn,
            gb_bridge::CiState::None => theme.muted,
        };
        let tip = format!("Pull request #{number} for {branch} is {state}. {checks}. {title}");
        div()
            .id(SharedString::from(format!("pr-row-{index}")))
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .cursor_pointer()
            .when(marked, |row| row.bg(theme.selected))
            .tooltip(Tooltip::text(tip))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.store.forge.cursor = index;
                cx.notify();
            }))
            .child(
                div()
                    .w(px(8.))
                    .h(px(8.))
                    .flex_none()
                    .rounded(px(4.))
                    .bg(dot),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .child(format!("#{number}")),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(state),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .child(branch),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_sm()
                    .truncate()
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(checks),
            )
            .into_any_element()
    }

    fn branch_page_row(
        &self,
        index: usize,
        row: &gb_bridge::BranchRow,
        marked: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let name = row.name.clone();
        let author = if row.author.is_empty() {
            "Unknown author".to_string()
        } else {
            row.author.clone()
        };
        let commit = if row.last_commit.is_empty() {
            "No commit yet".to_string()
        } else {
            row.last_commit.clone()
        };
        let tip_short = if row.tip_short.is_empty() {
            "-------".to_string()
        } else {
            row.tip_short.clone()
        };
        div()
            .id(SharedString::from(format!("branch-row-{index}")))
            .mx_2()
            .px_2()
            .py_1()
            .min_h(px(72.))
            .rounded_md()
            .flex()
            .flex_col()
            .gap(px(2.))
            .overflow_hidden()
            .border_1()
            .border_color(ring(marked, &theme))
            .when(marked, |row| row.bg(theme.selected))
            .cursor_pointer()
            .tooltip(Tooltip::text(format!(
                "{} · {} · {}",
                row.ref_name, commit, author
            )))
            .on_mouse_down(MouseButton::Right, {
                let name = name.clone();
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.store.place_branch(&name);
                    this.store.open_listed_branch(&name);
                    this.place_effect(
                        Effect::None,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        cx,
                    );
                })
            })
            .on_click({
                let name = name.clone();
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.store.place_branch(&name);
                    cx.notify();
                })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(ns_ui::CODE_FONT)
                            .text_sm()
                            .truncate()
                            .child(name.clone()),
                    )
                    .when(row.applied, |line| {
                        line.child(
                            div()
                                .id(SharedString::from(format!("applied-{index}")))
                                .flex_none()
                                .tooltip(Tooltip::text(format!("{name} is in the workspace")))
                                .child(ns_ui::chip("Applied".to_string(), Tone::Ok)),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(
                        div()
                            .flex_none()
                            .font_family(ns_ui::CODE_FONT)
                            .text_color(theme.accent)
                            .child(tip_short),
                    )
                    .child(div().flex_1().min_w(px(0.)).truncate().child(commit))
                    .child(div().flex_none().max_w(px(280.)).truncate().child(author)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(if let Some(label) = self.store.activity.row_label(&name) {
                        div()
                            .id(SharedString::from(format!("branch-busy-{index}")))
                            .h(px(20.))
                            .px_1()
                            .flex_none()
                            .flex()
                            .items_center()
                            .text_xs()
                            .text_color(theme.accent)
                            .tooltip(Tooltip::text(label.clone()))
                            .child(label)
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    .child(more_button(
                        SharedString::from(format!("branch-more-{index}")),
                        cx.listener({
                            let name = name.clone();
                            move |this, event: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.store.place_branch(&name);
                                this.store.open_listed_branch(&name);
                                let position = event.position();
                                this.place_effect(
                                    Effect::None,
                                    position.x.as_f32(),
                                    position.y.as_f32(),
                                    cx,
                                );
                            }
                        }),
                    )),
            )
            .into_any_element()
    }

    fn virtual_branches(&self) -> bool {
        if self.perf_full
            || self.store.pages.tab == BranchTab::PullRequests
            || self.store.pages.branches_error.is_some()
            || self
                .store
                .activity
                .surface_busy(store::Surface::BranchesPage)
        {
            return false;
        }
        let indexes = self.store.branch_indexes();
        if self.store.pages.branches_loading && indexes.is_empty() {
            return false;
        }
        !indexes.is_empty()
    }

    fn virtual_branch_rows(&self, cursor: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let indexes = self.store.branch_indexes();
        let count = indexes.len();
        let entity = cx.entity();
        let painted = Rc::clone(&self.painted);
        uniform_list("branch-rows", count, move |range, _, cx| {
            let indexes = Arc::clone(&indexes);
            painted.set(painted.get().saturating_add(range.len() as u64));
            entity.update(cx, |this, cx| {
                let mut out = Vec::new();
                for slot in range {
                    let Some(raw) = indexes.get(slot).copied() else {
                        continue;
                    };
                    let Some(row) = this.store.pages.branch_rows.get(raw).cloned() else {
                        continue;
                    };
                    out.push(this.branch_page_row(slot, &row, slot == cursor, cx));
                }
                out
            })
        })
        .track_scroll(&self.branch_list_scroll)
        .flex_1()
        .min_h(px(0.))
        .into_any_element()
    }

    fn virtual_history(&self) -> bool {
        if self.perf_full
            || self
                .store
                .activity
                .surface_busy(store::Surface::HistoryPage)
        {
            return false;
        }
        if self.store.pages.history_loading && self.store.pages.snapshots.is_empty() {
            return false;
        }
        !self.store.pages.snapshots.is_empty()
    }

    fn virtual_snapshots(&self, cursor: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let count = self.store.pages.snapshots.len();
        let entity = cx.entity();
        let painted = Rc::clone(&self.painted);
        uniform_list("history-rows", count, move |range, _, cx| {
            painted.set(painted.get().saturating_add(range.len() as u64));
            entity.update(cx, |this, cx| {
                let mut out = Vec::new();
                for index in range {
                    let Some(snapshot) = this.store.pages.snapshots.get(index).cloned() else {
                        continue;
                    };
                    out.push(this.snapshot_row(index, &snapshot, index == cursor, cx));
                }
                out
            })
        })
        .track_scroll(&self.history_list_scroll)
        .flex_1()
        .min_h(px(0.))
        .into_any_element()
    }

    fn snapshot_row(
        &self,
        index: usize,
        snapshot: &gb_bridge::SnapshotInfo,
        marked: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let title = snapshot.title.clone();
        let when = snapshot_when(snapshot.created_at_ms);
        let kind = snapshot_kind(&snapshot.operation);
        let sha = snapshot.sha.clone();
        div()
            .id(SharedString::from(format!("snapshot-{index}")))
            .min_h(px(36.))
            .px_2()
            .rounded_md()
            .flex()
            .flex_col()
            .justify_center()
            .overflow_hidden()
            .cursor_pointer()
            .border_1()
            .border_color(ring(marked, &theme))
            .when(marked, |row| row.bg(theme.selected))
            .tooltip(Tooltip::text(format!("{title} · {when} · {kind}")))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let effect = this.store.select_snapshot_id(&sha);
                this.finish_page_request(effect, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("snapshot-kind-{index}")))
                            .flex_none()
                            .tooltip(Tooltip::text(format!("Kind: {kind}")))
                            .child(ns_ui::chip(kind.to_string(), Tone::Neutral)),
                    ),
            )
            .child(div().text_xs().text_color(theme.muted).child(when))
            .into_any_element()
    }

    fn history_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let virtual_history = self.virtual_history();
        // Borrowed, not cloned: neither list is mutated while building rows.
        let snapshots = &self.store.pages.snapshots;
        let cursor = self.store.pages.snapshot_cursor;
        let message = self.store.pages.snapshot_message.clone();
        let focused = self.store.pages.message_focused;
        let notice = self.store.pages.history_notice.clone().unwrap_or_default();
        let notice_bad = self.store.pages.history_notice_bad;
        let files = Arc::clone(&self.store.pages.snapshot_files);
        let diff_loading = self.store.pages.diff_loading;
        let selected = self.store.pages.selected_snapshot().cloned();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if self
            .store
            .activity
            .surface_busy(store::Surface::HistoryPage)
            || (self.store.pages.history_loading
                && snapshots.is_empty()
                && self.store.activity.spinner_on("list_snapshots"))
        {
            rows.push(ns_ui::loading_pane("history-loading", "Loading history").into_any_element());
        } else if self.store.pages.history_loading && snapshots.is_empty() {
            rows.push(quiet_body());
        } else if virtual_history {
        } else if snapshots.is_empty() {
            if let Some(message) = &self.store.pages.history_error {
                rows.push(self.error_note("history-error", message, "list_snapshots", cx));
            } else {
                rows.push(
                    div()
                        .id("history-empty")
                        .px_2()
                        .py_2()
                        .text_xs()
                        .text_color(theme.muted)
                        .tooltip(Tooltip::text(
                            "No snapshots yet. Type a sentence above, then choose Create snapshot.",
                        ))
                        .child(
                            "No snapshots yet. Create one to record the project as it is now. Type a sentence above, then choose Create snapshot.",
                        )
                        .into_any_element(),
                );
            }
        } else {
            for (index, snapshot) in snapshots.iter().enumerate() {
                rows.push(self.snapshot_row(index, snapshot, index == cursor, cx));
            }
        }
        let mut file_rows: Vec<gpui::AnyElement> = Vec::new();
        if diff_loading {
            file_rows.push(
                div()
                    .text_xs()
                    .text_color(theme.muted)
                    .child("Reading changed files")
                    .into_any_element(),
            );
        } else if selected.is_none() {
            file_rows.push(
                div()
                    .text_xs()
                    .text_color(theme.muted)
                    .child("Select a snapshot to see its changed files.")
                    .into_any_element(),
            );
        } else if files.is_empty() {
            file_rows.push(
                div()
                    .id("snapshot-files-empty")
                    .text_xs()
                    .text_color(theme.muted)
                    .child("No file changes in this snapshot.")
                    .into_any_element(),
            );
        } else {
            // The 88-point pane shows about four 22-point rows; the fixed
            // pane never scrolls, so the visible window is all it can paint.
            let window = store::visible_window(
                files.len(),
                0.0,
                SNAPSHOT_FILES_PX,
                SNAPSHOT_FILE_PITCH_PX,
                0,
            );
            let window_start = window.start;
            for (offset, change) in files[window].iter().enumerate() {
                let index = window_start + offset;
                let path = change.path.clone();
                let status = change.status;
                file_rows.push(
                    div()
                        .id(SharedString::from(format!("snapshot-file-{index}")))
                        .h(px(22.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .overflow_hidden()
                        .font_family(ns_ui::CODE_FONT)
                        .text_xs()
                        .tooltip(Tooltip::text(format!(
                            "{} {path}",
                            store::file_change_label(status)
                        )))
                        .child(status_mark(status))
                        .child(div().min_w(px(0.)).truncate().child(path))
                        .into_any_element(),
                );
            }
        }
        let draft = if message.is_empty() && !focused {
            "Snapshot message".to_string()
        } else if focused {
            format!("{message}▌")
        } else {
            message.clone()
        };
        let can_restore = selected
            .as_ref()
            .is_some_and(|snapshot| snapshot.restorable)
            && self.store.mutation_blocked().is_none()
            && !self.store.pages.history_busy;
        let restore_tip = if self.store.mutation_blocked().is_some() {
            self.store.mutation_blocked().unwrap().to_string()
        } else if selected
            .as_ref()
            .is_some_and(|snapshot| !snapshot.restorable)
        {
            "Undo and redo entries are not restored from here. Use Undo.".to_string()
        } else if selected.is_none() {
            "Select a snapshot first.".to_string()
        } else {
            "Put the project back to the selected snapshot. This asks before it writes.".to_string()
        };
        div()
            .id("history-panel")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.background)
            .child(
                div()
                    .h(px(40.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(div().text_sm().child("History"))
                    .child(
                        div()
                            .id("history-hint")
                            .text_xs()
                            .text_color(theme.muted)
                            .tooltip(Tooltip::text(
                                "Escape returns to the workspace. b opens Branches.",
                            ))
                            .child("Escape returns to the workspace"),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .id("snapshot-message")
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(28.))
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(ring(focused, &theme))
                            .bg(theme.editor)
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .font_family(ns_ui::CODE_FONT)
                            .text_xs()
                            .text_color(if message.is_empty() && !focused {
                                theme.muted
                            } else {
                                theme.text
                            })
                            .cursor_pointer()
                            .tooltip(Tooltip::text(
                                "Message for the new snapshot. Enter records it. Escape leaves the field.",
                            ))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.store.pages.message_focused = true;
                                cx.notify();
                            }))
                            .child(div().min_w(px(0.)).truncate().child(draft)),
                    )
                    .child(self.snapshot_create_button(cx)),
            )
            .child(notice_line("history-notice", &notice, notice_bad))
            .child(if virtual_history {
                self.virtual_snapshots(cursor, cx)
            } else {
                if self.perf_full {
                    self.painted
                        .set(self.painted.get().saturating_add(rows.len() as u64));
                }
                ScrollArea::new("history-scroll")
                    .vertical()
                    .track_scroll(&self.history_scroll)
                    .flex_1()
                    .min_h(px(0.))
                    .child(div().w_full().p_1().flex().flex_col().gap(px(2.)).children(rows))
                    .into_any_element()
            })
            .child(
                div()
                    .flex_none()
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted)
                    .child("Changed files"),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(88.))
                    .px_2()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(file_rows),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(36.))
                    .px_2()
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(if can_restore {
                        word_button(
                            SharedString::from("restore-snapshot"),
                            "Restore to here",
                            restore_tip,
                            true,
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Err(error) = this.store.ask_restore() {
                                    this.store.toast_bad = true;
                                    this.store.toast = Some(error);
                                }
                                cx.notify();
                            }),
                        )
                    } else {
                        inert_word(
                            SharedString::from("restore-snapshot"),
                            "Restore to here",
                            restore_tip,
                        )
                    }),
            )
    }

    fn snapshot_create_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(reason) = self.store.mutation_blocked() {
            return inert_word(
                SharedString::from("create-snapshot"),
                "Create snapshot",
                reason.to_string(),
            );
        }
        if self.store.pages.history_busy {
            return wait_slot(
                SharedString::from("create-snapshot-busy"),
                "Recording the snapshot".to_string(),
            );
        }
        word_button(
            SharedString::from("create-snapshot"),
            "Create snapshot",
            "Record the project as it is now. An empty message is labeled Created snapshot."
                .to_string(),
            false,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let message = this.store.pages.snapshot_message.clone();
                let effect = this.store.create_snapshot_now(&message);
                this.finish_page_request(effect, cx);
            }),
        )
    }

    fn conflict_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let title = self.store.pages.conflict_title.clone();
        let short = self
            .store
            .pages
            .conflict_sha
            .as_deref()
            .map(short7)
            .unwrap_or_default();
        let files = self.store.pages.conflict_files.clone();
        let cursor = self.store.pages.conflict_cursor;
        let notice = self.store.pages.conflict_notice.clone().unwrap_or_default();
        let notice_bad = self.store.pages.conflict_notice_bad;
        let busy = self.store.pages.conflict_busy;
        let ready = !files.is_empty() && files.iter().all(|file| file.side.is_some());
        let locked = self.store.mutation_blocked().map(str::to_string);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if busy && files.is_empty() {
            rows.push(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted)
                    .child("Reading conflicted files")
                    .into_any_element(),
            );
        } else if files.is_empty() {
            rows.push(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted)
                    .child("This commit has no files to resolve. Finish it in edit mode.")
                    .into_any_element(),
            );
        } else {
            for (index, file) in files.iter().enumerate() {
                rows.push(self.conflict_file_row(index, file, index == cursor, cx));
            }
        }
        let mark_tip = if let Some(reason) = &locked {
            reason.clone()
        } else if !ready {
            "Choose ours or theirs for every file.".to_string()
        } else {
            "Keep the chosen side of every file. The commit id changes.".to_string()
        };
        div()
            .id("conflict-panel")
            .absolute()
            .top(px(84.))
            .left(px(8.))
            .w(px(400.))
            .max_h(px(520.))
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(32.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .id("conflict-title")
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .truncate()
                            .tooltip(Tooltip::text(format!(
                                "Resolve conflicts in {short} {title}"
                            )))
                            .child(format!("Resolve conflicts · {short}")),
                    )
                    .child(
                        IconButton::new("conflict-close", IconName::X)
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .tooltip("Close conflict resolution")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.store.close_conflicts();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("conflict-legend")
                    .px_2()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted)
                    .truncate()
                    .tooltip(Tooltip::text(
                        "Ours keeps the new base. Theirs keeps this commit's own version.",
                    ))
                    .child("Ours keeps the new base. Theirs keeps this commit's version."),
            )
            .child(notice_line("conflict-notice", &notice, notice_bad))
            .child(
                ScrollArea::new("conflict-scroll")
                    .vertical()
                    .flex_1()
                    .min_h(px(80.))
                    .child(
                        div()
                            .w_full()
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(rows),
                    ),
            )
            .child(
                div()
                    .h(px(36.))
                    .px_2()
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(if ready && locked.is_none() && !busy {
                        word_button(
                            SharedString::from("mark-resolved"),
                            "Mark resolved",
                            mark_tip,
                            false,
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                let effect = this.store.mark_resolved();
                                this.finish_page_request(effect, cx);
                            }),
                        )
                    } else {
                        inert_word(
                            SharedString::from("mark-resolved"),
                            "Mark resolved",
                            mark_tip,
                        )
                    }),
            )
    }

    fn conflict_file_row(
        &self,
        index: usize,
        file: &store::ConflictFile,
        marked: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let path = file.path.clone();
        let ours = file.side == Some(gb_bridge::ConflictChoice::Ours);
        let theirs = file.side == Some(gb_bridge::ConflictChoice::Theirs);
        let path_for_ours = path.clone();
        let path_for_theirs = path.clone();
        div()
            .id(SharedString::from(format!("conflict-file-{index}")))
            .min_h(px(32.))
            .px_2()
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .border_1()
            .border_color(ring(marked, &theme))
            .when(marked, |row| row.bg(theme.selected))
            .child(
                div()
                    .id(SharedString::from(format!("conflict-path-{index}")))
                    .flex_1()
                    .min_w(px(0.))
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .truncate()
                    .tooltip(Tooltip::text(path.clone()))
                    .child(path),
            )
            .child(self.side_choice(index, "ours", "Ours", ours, path_for_ours, cx))
            .child(self.side_choice(index, "theirs", "Theirs", theirs, path_for_theirs, cx))
            .into_any_element()
    }

    fn side_choice(
        &self,
        index: usize,
        side: &'static str,
        label: &'static str,
        on: bool,
        path: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ns_ui::theme();
        let tip = if side == "ours" {
            format!("Keep the new base for {path}")
        } else {
            format!("Keep this commit's version of {path}")
        };
        if let Some(reason) = self.store.mutation_blocked() {
            return inert_word(
                SharedString::from(format!("side-{side}-{index}")),
                label,
                reason.to_string(),
            );
        }
        div()
            .id(SharedString::from(format!("side-{side}-{index}")))
            .h(px(22.))
            .px_2()
            .flex_none()
            .rounded_sm()
            .flex()
            .items_center()
            .text_xs()
            .cursor_pointer()
            .border_1()
            .border_color(if on {
                gpui::Hsla::from(theme.accent)
            } else {
                gpui::Hsla::from(theme.border)
            })
            .when(on, |button| {
                button.bg(theme.selected).text_color(theme.accent)
            })
            .tooltip(Tooltip::text(tip))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                if let Err(error) = this.store.choose_side(&path, side) {
                    this.store.toast_bad = true;
                    this.store.toast = Some(error);
                }
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    fn page_ask(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(ask) = self.store.pages.ask.clone() else {
            return div().into_any_element();
        };
        let theme = ns_ui::theme();
        let confirm_icon = if ask.confirm_label.to_ascii_lowercase().contains("delete") {
            "trash"
        } else {
            "history"
        };
        let banner = ns_ui::Ask {
            title: ask.title.into(),
            message: ask.message.into(),
            confirm: ask.confirm_label.into(),
            confirm_icon,
            destructive: ask.destructive,
        };
        div()
            .id("page-ask")
            .absolute()
            .bottom(px(48.))
            .left(px(12.))
            .w(px(440.))
            .rounded_md()
            .border_1()
            .border_color(if ask.destructive {
                theme.err
            } else {
                theme.border
            })
            .occlude()
            .shadow(cx.theme().elevation(Elevation::Floating))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(ns_ui::ask_banner(
                &banner,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    let effect = this.store.confirm_page_yes();
                    this.activate(effect, cx);
                    cx.notify();
                }),
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.store.confirm_page_no();
                    cx.notify();
                }),
            ))
            .into_any_element()
    }

    fn finish_page_request(&mut self, result: Result<Effect, String>, cx: &mut Context<Self>) {
        match result {
            Ok(effect) => self.activate(effect, cx),
            Err(error) => {
                self.store.toast_bad = true;
                self.store.toast = Some(error);
            }
        }
        cx.notify();
    }

    fn settings_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let section = self.store.settings.section;
        let cursor = self.store.settings.cursor;
        let global = self.store.settings.global.clone();
        let project = self.store.settings.project.clone();
        let project_error = self.store.settings.project_error.clone();
        let load_error = self.store.settings.error.clone();
        let fetch_draft = self.store.settings.fetch_draft.clone();
        let fetch_focused = self.store.settings.fetch_focused;
        let status = self.store.settings.status.clone().unwrap_or_default();
        let status_bad = self.store.settings.status_bad;
        let confirm = self.store.settings.confirm_remove;
        let body = if self.store.settings.loading && global.is_none() {
            if self.store.activity.spinner_on("read_global") {
                ns_ui::loading_pane("settings-loading", "Reading settings").into_any_element()
            } else {
                quiet_body()
            }
        } else if global.is_none() {
            let message = load_error
                .clone()
                .unwrap_or_else(|| "Settings could not be read.".to_string());
            self.error_note("settings-error", &message, "read_global", cx)
        } else if section == SettingsSection::App {
            app_settings_rows(
                global.as_ref().expect("global"),
                cursor,
                &fetch_draft,
                fetch_focused,
                cx,
            )
        } else {
            project_settings_rows(project.as_ref(), project_error.as_deref(), cursor, cx)
        };
        div()
            .id("settings-page")
            .absolute()
            .top(px(72.))
            .left(px(0.))
            .right(px(0.))
            .bottom(px(24.))
            .occlude()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.background)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_none()
                    .h(px(40.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(div().text_sm().child("Settings"))
                    .child(self.settings_tab(SettingsSection::App, cx))
                    .child(self.settings_tab(SettingsSection::Project, cx))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("settings-close", IconName::X)
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .tooltip("Close settings")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.store.close_settings();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                ScrollArea::new("settings-scroll")
                    .vertical()
                    .flex_1()
                    .min_h(px(0.))
                    .child(body),
            )
            .child(notice_line("settings-status", &status, status_bad))
            .when(confirm, |page| page.child(self.remove_confirm(cx)))
    }

    fn settings_tab(&self, section: SettingsSection, cx: &mut Context<Self>) -> gpui::AnyElement {
        let on = self.store.settings.section == section;
        let (label, tip) = match section {
            SettingsSection::App => ("App", "App settings: telemetry, fetch, and appearance"),
            SettingsSection::Project => (
                "Project",
                "This project's target branch, signing, and removal",
            ),
        };
        let id = match section {
            SettingsSection::App => "settings-tab-app",
            SettingsSection::Project => "settings-tab-project",
        };
        if on {
            inert_word(SharedString::from(id), label, tip.to_string())
        } else {
            word_button(
                SharedString::from(id),
                label,
                tip.to_string(),
                false,
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.store.show_settings_section(section);
                    cx.notify();
                }),
            )
        }
    }

    fn remove_confirm(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        div()
            .id("settings-remove-confirm")
            .absolute()
            .bottom(px(36.))
            .left(px(12.))
            .w(px(440.))
            .p_3()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.err)
            .occlude()
            .flex()
            .flex_col()
            .gap_2()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().text_sm().child("Remove this project from the app?"))
            .child(div().text_xs().text_color(theme.muted).child(
                "The repository folder stays on disk. This only drops it from the project list.",
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(word_button(
                        SharedString::from("settings-remove-yes"),
                        "Remove project",
                        "Remove this project from the app. The files stay where they are."
                            .to_string(),
                        true,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            let effect = this
                                .store
                                .apply_settings_key("enter")
                                .unwrap_or(Effect::None);
                            this.activate(effect, cx);
                            cx.notify();
                        }),
                    ))
                    .child(word_button(
                        SharedString::from("settings-remove-no"),
                        "Cancel",
                        "Keep the project in the app".to_string(),
                        false,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.store.cancel_remove_project();
                            cx.notify();
                        }),
                    )),
            )
    }

    fn help_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ns_ui::theme();
        let rows: Vec<_> = shortcut_rows()
            .iter()
            .enumerate()
            .map(|(index, row)| help_line(index, row.keys, row.what, row.tokens))
            .collect();
        div()
            .id("help-card")
            .absolute()
            .top(px(72.))
            .bottom(px(36.))
            .right(px(12.))
            .w(px(460.))
            .p_3()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .shadow(cx.theme().elevation(Elevation::Floating))
            .occlude()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().child("Keyboard"))
                    .child(
                        IconButton::new("help-close", IconName::X)
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .tooltip("Close keyboard help")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.store.help = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                ScrollArea::new("help-scroll")
                    .vertical()
                    .flex_1()
                    .min_h(px(0.))
                    .child(div().flex().flex_col().gap_1().children(rows)),
            )
    }

    fn maybe_start_env_perf(&mut self, cx: &mut Context<Self>) {
        if self.perf_armed || !self.preview || std::env::var_os("GB_GPUI_PERF").is_none() {
            return;
        }
        if self.store.loading || self.store.selected.is_none() {
            return;
        }
        self.perf_armed = true;
        let label = std::env::var("GB_GPUI_PERF_LABEL").unwrap_or_else(|_| "after".to_string());
        let label = if label == "before" { "before" } else { "after" };
        let full = label == "before" || std::env::var_os("GB_GPUI_PERF_FULL").is_some();
        if let Ok(names) = perf_harness::perf_names("all") {
            self.start_perf(label, names, full, cx);
        }
    }

    fn start_perf(&mut self, label: &str, names: Vec<String>, full: bool, cx: &mut Context<Self>) {
        self.perf_full = full;
        if let Ok(mut slot) = self.perf_headless.lock() {
            *slot = None;
        }
        self.kick_headless(cx);
        {
            let mut run = self.perf.borrow_mut();
            run.recording = true;
            run.frame_armed = false;
            run.label = label.to_string();
            run.full = full;
            run.names = names;
            run.index = 0;
            run.samples.clear();
            run.wrote = false;
            run.error = None;
            run.path.clear();
        }
        let first = self
            .perf
            .borrow()
            .names
            .first()
            .cloned()
            .unwrap_or_default();
        self.prepare_scenario(&first);
        cx.notify();
        self.kick_perf_draw(cx);
    }

    fn kick_headless(&self, cx: &mut Context<Self>) {
        let slot = Arc::clone(&self.perf_headless);
        cx.background_spawn(async move {
            let value = store::perf_measure();
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(value);
            }
        })
        .detach();
    }

    fn prepare_scenario(&mut self, name: &str) {
        self.seed_perf(name);
        let target = match name {
            "idle" => 12,
            "scroll-long-diff" => 24,
            "long-file-list" => 12,
            "long-history" => 32,
            "watcher-burst" => 24,
            "column-drag" => 24,
            "large-tree" | "large-list-marked" | "branch-detail" | "branch-search" | "pr-tab"
            | "snapshot-files" => 60,
            _ => 8,
        };
        let paint_base = self.painted.get();
        let notify_base = self.notifies.get();
        let gate_base = self.store.refresh_gate.started();
        let mut run = self.perf.borrow_mut();
        run.current = name.to_string();
        run.frames_target = target;
        run.frames_done = 0;
        run.renders.clear();
        run.started = Instant::now();
        run.paint_base = paint_base;
        run.notify_base = notify_base;
        run.gate_base = gate_base;
    }

    fn seed_perf(&mut self, name: &str) {
        self.store.pages.branches_open = false;
        self.store.pages.history_open = false;
        self.store.pages.conflicts_open = false;
        self.store.settings.open = false;
        self.store.help = false;
        self.store.project_menu = false;
        match name {
            "scroll-long-diff" => {
                self.store.diff_rows = Arc::new(store::seed_diff(5_000));
                self.store.shown = ShownFile::Worktree {
                    path: "src/long.diff".to_string(),
                };
                self.store.diff_loading = false;
                self.store.diff_error = None;
                self.store.pane = Pane::Changes;
            }
            "long-file-list" => {
                self.store
                    .set_worktree(Some(Arc::new(store::seed_files(2_000))));
                self.store.view = ViewMode::List;
                self.store.pane = Pane::Uncommitted;
                self.store.loading = false;
            }
            "long-history" => {
                self.store
                    .set_workspace(Some(Arc::new(store::seed_workspace(2_000))));
                self.store.pages.snapshots = store::seed_snapshots(2_000);
                self.store.pages.history_loading = false;
                self.store.pages.history_error = None;
                self.store.loading = false;
            }
            "watcher-burst" => {
                self.store
                    .set_workspace(Some(Arc::new(store::seed_workspace(4))));
                self.store
                    .set_worktree(Some(Arc::new(store::seed_files(6))));
                self.store.pages.snapshots.clear();
                self.store.diff_rows = Arc::new(Vec::new());
                self.store.shown = ShownFile::None;
                self.store.loading = false;
                self.store.refreshing = false;
                // Already quiet, so the workspace pump can start one refresh
                // while the later draws of this scenario are still running.
                let due = self.elapsed_ms().saturating_sub(store::REFRESH_QUIET_MS);
                for _ in 0..200 {
                    self.store.apply_event(store::seed_git_event());
                    self.store.refresh_gate.note(due);
                }
            }
            "column-drag" => {
                self.store.column_fractions = store::COLUMN_DEFAULT;
            }
            "large-tree" => self.seed_large_marked(ViewMode::Tree),
            "large-list-marked" => self.seed_large_marked(ViewMode::List),
            "branch-detail" => {
                self.store
                    .set_workspace(Some(Arc::new(store::seed_detail_workspace())));
                self.store.side = SideFocus::Branch {
                    stack: 0,
                    branch: 0,
                };
                self.store.pane = Pane::Detail;
                self.store.commit_loading = false;
                self.store.commit_error = None;
            }
            "branch-search" => {
                self.store.set_branch_rows(store::seed_large_branches());
                self.store.pages.tab = BranchTab::Local;
                self.store.pages.filter.clear();
                self.store.pages.branches_open = true;
                self.store.pages.branches_loading = false;
                self.store.pages.branches_error = None;
                self.store.pages.branch_notice = None;
            }
            "pr-tab" => {
                self.store.set_branch_rows(store::seed_large_branches());
                self.store.pages.tab = BranchTab::PullRequests;
                self.store.pages.filter = "topic".to_string();
                self.store.pages.branches_open = true;
                self.store.pages.branches_loading = false;
                self.store.pages.branches_error = None;
                self.store.pages.branch_notice = None;
                self.store.forge.connected = true;
                self.store.set_reviews(store::seed_large_reviews());
            }
            "snapshot-files" => {
                self.store.pages.snapshots = store::seed_snapshots(2_000);
                self.store.pages.snapshot_cursor = 0;
                self.store.pages.snapshot_files = Arc::new(store::seed_large_snapshot_files());
                self.store.pages.history_open = true;
                self.store.pages.history_loading = false;
                self.store.pages.history_error = None;
                self.store.pages.diff_loading = false;
            }
            _ => {}
        }
    }

    /// A 5,000-file worktree in ~300 dirs with ~40% of files and hunks marked,
    /// so `commit_summary`, `marked_specs`, and `folder_fully_marked` do real
    /// work while the composer stays collapsed.
    fn seed_large_marked(&mut self, view: ViewMode) {
        const MARKED_FILES: usize = 2_000;
        let worktree = store::seed_large_files();
        let mut marked = std::collections::BTreeSet::new();
        let mut marked_hunks = std::collections::BTreeSet::new();
        for file in worktree.files.iter().take(MARKED_FILES) {
            marked.insert(file.path.clone());
            for hunk in &file.hunks {
                marked_hunks.insert((file.path.clone(), hunk.header.clone()));
            }
        }
        self.store.set_worktree(Some(Arc::new(worktree)));
        self.store
            .set_workspace(Some(Arc::new(store::seed_large_workspace())));
        self.store.view = view;
        self.store.pane = Pane::Uncommitted;
        self.store.side = SideFocus::None;
        self.store.loading = false;
        self.store.refreshing = false;
        self.store.composer.open = false;
        self.store.composer.field = None;
        self.store.set_marks(marked, marked_hunks);
        self.store.collapsed.clear();
        self.store.dirs_collapsed.clear();
        self.store.shown = ShownFile::None;
        self.store.diff_rows = Arc::new(Vec::new());
        self.store.diff_loading = false;
        self.store.diff_error = None;
    }

    /// Count the draw that just finished. Its sample is already in `renders`.
    fn settle_recorded_frame(&mut self, _cx: &mut Context<Self>) {
        let (recording, recorded, done, target) = {
            let run = self.perf.borrow();
            (
                run.recording,
                u32::try_from(run.renders.len()).unwrap_or(u32::MAX),
                run.frames_done,
                run.frames_target,
            )
        };
        if !recording || recorded == done {
            return;
        }
        self.mark_notify();
        self.perf.borrow_mut().frames_done = recorded;
        if recorded >= target {
            self.close_scenario();
            if !self.open_next_scenario() {
                self.finish_perf();
            }
        } else {
            self.step_scenario();
        }
    }

    /// Schedule one more draw. A covered window stops the display link, so
    /// `on_next_frame` never runs there. The timer lets this render return
    /// before the next `draw`.
    fn kick_perf_draw(&self, cx: &mut Context<Self>) {
        let Some(handle) = cx.windows().into_iter().next() else {
            return;
        };
        let wait_ms = {
            let run = self.perf.borrow();
            if run.current == "watcher-burst" && run.frames_done == 0 {
                220
            } else {
                1
            }
        };
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(wait_ms))
                .await;
            cx.update(|app| {
                let _ = app.update_window(handle, |_, window, cx| {
                    window.draw(cx).clear(cx);
                });
            });
        })
        .detach();
    }

    fn step_scenario(&mut self) {
        let (name, done) = {
            let run = self.perf.borrow();
            (run.current.clone(), run.frames_done)
        };
        match name.as_str() {
            "scroll-long-diff" => {
                let len = self.store.diff_rows.len();
                if len > 0 {
                    let index = (done as usize).saturating_mul(120).min(len - 1);
                    self.diff_scroll
                        .scroll_to_item_strict(index, ScrollStrategy::Top);
                }
            }
            "long-file-list" => {
                let len = self.store.file_slots().len();
                if len > 0 {
                    let index = (done as usize).saturating_mul(80).min(len - 1);
                    self.file_scroll
                        .scroll_to_item_strict(index, ScrollStrategy::Top);
                }
            }
            "long-history" => {
                if done == 16 {
                    self.store.pages.branches_open = false;
                    self.store.pages.history_open = true;
                    self.store.pages.history_loading = false;
                }
                if done < 16 {
                    let len = self.store.lane_rows().len();
                    if len > 0 {
                        let index = (done as usize).saturating_mul(80).min(len - 1);
                        self.lane_scroll
                            .scroll_to_item_strict(index, ScrollStrategy::Top);
                    }
                } else {
                    let len = self.store.pages.snapshots.len();
                    if len > 0 {
                        let index = (done.saturating_sub(16) as usize).saturating_mul(80);
                        let index = index.min(len - 1);
                        self.history_list_scroll
                            .scroll_to_item_strict(index, ScrollStrategy::Top);
                    }
                }
            }
            "column-drag" => {
                let delta = if done % 2 == 0 { 16.0 } else { -16.0 };
                let viewport = self.viewport_width.max(640.0);
                self.store.column_fractions = store::drag_handle(
                    self.store.column_fractions,
                    (done as usize) % 2,
                    delta,
                    viewport,
                );
            }
            "large-tree" | "large-list-marked" => {
                let len = match self.store.view {
                    ViewMode::Tree => self.store.tree_rows().len(),
                    ViewMode::List => self.store.file_slots().len(),
                };
                if len > 0 {
                    let index = (done as usize).saturating_mul(60).min(len - 1);
                    self.file_scroll
                        .scroll_to_item_strict(index, ScrollStrategy::Top);
                }
            }
            "branch-detail" => {
                let len = self.store.lane_rows().len();
                if len > 0 {
                    let index = (done as usize).saturating_mul(60).min(len - 1);
                    self.lane_scroll
                        .scroll_to_item_strict(index, ScrollStrategy::Top);
                }
            }
            "branch-search" => {
                // Type "topic-01" one char per frame, then backspace it off,
                // so every frame re-filters the 3,000 rows the way typing does.
                const QUERY: [char; 8] = ['t', 'o', 'p', 'i', 'c', '-', '0', '1'];
                let phase = done as usize % (QUERY.len() * 2);
                let key = if phase < QUERY.len() {
                    QUERY[phase].to_string()
                } else {
                    "backspace".to_string()
                };
                let _ = self.store.apply_key(&key);
            }
            "pr-tab" => {
                // The PR tab renders every matching review in a plain scroll
                // area; move it so each frame paints a different window.
                let y = (done as f32 * 60.0 * 36.0).min(36_000.0);
                self.branch_scroll.set_offset(point(px(0.), px(-y)));
            }
            "snapshot-files" => {
                let len = self.store.pages.snapshots.len();
                if len > 0 {
                    let index = (done as usize).saturating_mul(60).min(len - 1);
                    self.history_list_scroll
                        .scroll_to_item_strict(index, ScrollStrategy::Top);
                }
            }
            _ => {}
        }
    }

    fn close_scenario(&mut self) {
        let (name, frames, elapsed_ms, renders, paint_base, notify_base, gate_base) = {
            let run = self.perf.borrow();
            let elapsed_ms = u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            (
                run.current.clone(),
                run.frames_done,
                elapsed_ms,
                run.renders.clone(),
                run.paint_base,
                run.notify_base,
                run.gate_base,
            )
        };
        let painted = self.painted.get().saturating_sub(paint_base);
        let notifies = self.notifies.get().saturating_sub(notify_base);
        let refreshes = self.store.refresh_gate.started().saturating_sub(gate_base);
        let sample = perf_harness::scenario_sample(
            &name, frames, elapsed_ms, &renders, painted, notifies, refreshes,
        );
        self.perf.borrow_mut().samples.push(sample);
    }

    fn open_next_scenario(&mut self) -> bool {
        let next = {
            let mut run = self.perf.borrow_mut();
            let next_index = run.index.saturating_add(1);
            if next_index >= run.names.len() {
                return false;
            }
            run.index = next_index;
            run.names[next_index].clone()
        };
        self.prepare_scenario(&next);
        true
    }

    fn finish_perf(&mut self) {
        let (label, full, samples) = {
            let run = self.perf.borrow();
            (run.label.clone(), run.full, run.samples.clone())
        };
        let headless = self
            .perf_headless
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(store::perf_measure);
        let path = perf_harness::output_path(&label);
        let payload = serde_json::json!({
            "label": label,
            "full": full,
            "profile": perf_harness::profile_name(),
            "machine": perf_harness::machine_info(),
            "scenarios": samples,
            "headless": headless,
            "notes": "render_us is element-build time inside render(). fps is completed scenario frames over wall time. A covered window does not get a display link, so the run schedules the next draw itself after each render. painted_rows counts rows walked while drawing. notifies counts the pump, refresh completion, and perf steps. full=true paints every file, branch, and history row and scans every diff line. The headless watcher numbers are the coalescing proof.",
        });
        let write_error = perf_harness::write_report(&path, &payload).err();
        self.perf_full = std::env::var_os("GB_GPUI_PERF_FULL").is_some();
        let mut run = self.perf.borrow_mut();
        run.recording = false;
        run.wrote = write_error.is_none();
        run.path = path.display().to_string();
        run.error = write_error;
    }

    #[cfg(feature = "devtools")]
    fn perf_status(&self) -> serde_json::Value {
        let run = self.perf.borrow();
        serde_json::json!({
            "recording": run.recording,
            "label": run.label,
            "full": run.full,
            "scenario": run.current,
            "index": run.index,
            "scenarios": run.names.len(),
            "frames_done": run.frames_done,
            "frames_target": run.frames_target,
            "refresh_in_flight": self.store.refresh_gate.in_flight(),
            "group_rebuilds": self.store.group_rebuilds(),
            "lane_rebuilds": self.store.lane_rebuilds(),
            "branch_filter_rebuilds": self.store.branch_filter_rebuilds(),
            "wrote": run.wrote,
            "path": run.path,
            "error": run.error,
        })
    }
}

struct HunkChrome {
    path: String,
    header: String,
    marked: bool,
    busy: bool,
    entity: gpui::Entity<GbWindow>,
}

fn paint_diff_row(
    row: &DiffRow,
    index: usize,
    selected: bool,
    chrome: Option<HunkChrome>,
    text: SharedString,
) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    let (fg, bg) = match row.kind {
        RowKind::Add => (theme.ok, Some(theme.ok_bg)),
        RowKind::Delete => (theme.err, Some(theme.err_bg)),
        RowKind::Hunk => (theme.muted, Some(theme.raised)),
        RowKind::Note => (theme.warn, None),
        RowKind::Context => (theme.text, None),
    };
    let lane_tip = row
        .lane
        .as_ref()
        .map(|lane| format!("Assigned to {lane}"))
        .unwrap_or_else(|| row.text.clone());
    let hunk = row.kind == RowKind::Hunk;
    div()
        .id(SharedString::from(format!("diff-line-{index}")))
        .h(px(26.))
        .w_full()
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .overflow_hidden()
        .font_family(ns_ui::CODE_FONT)
        .text_xs()
        .text_color(fg)
        .border_1()
        .border_color(ring(selected, &theme))
        .when(selected, |line| line.bg(theme.selected))
        .when(!selected, |line| line.when_some(bg, |line, bg| line.bg(bg)))
        .tooltip(Tooltip::text(lane_tip))
        .when(hunk && chrome.is_some(), |line| {
            let chrome = chrome.as_ref().expect("hunk");
            let path = chrome.path.clone();
            let header = chrome.header.clone();
            let entity = chrome.entity.clone();
            line.on_mouse_down(MouseButton::Right, move |event: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let x = event.position.x.as_f32();
                let y = event.position.y.as_f32();
                entity.update(cx, |this, cx| {
                    this.store.pane = Pane::Changes;
                    this.store.set_cursor(Pane::Changes, index);
                    let effect = this.store.open_hunk_actions(&path, &header);
                    this.place_effect(effect, x, y, cx);
                });
            })
        })
        .child(gutter(row.old_line))
        .child(gutter(row.new_line))
        .child(div().flex_1().min_w(px(0.)).truncate().child(text))
        .when_some(row.lane.clone(), |line, lane| {
            line.when(hunk, |line| {
                line.child(
                    div()
                        .id(SharedString::from(format!("hunk-lane-{index}")))
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.selected)
                        .text_color(theme.accent)
                        .tooltip(Tooltip::text(format!("Hunk assigned to {lane}")))
                        .child(format!("→ {lane}")),
                )
            })
        })
        .when_some(chrome.filter(|_| hunk), |line, chrome| {
            line.child(hunk_actions(index, chrome))
        })
        .into_any_element()
}

fn hunk_actions(index: usize, chrome: HunkChrome) -> gpui::AnyElement {
    let path = chrome.path.clone();
    let header = chrome.header.clone();
    let marked = chrome.marked;
    let busy = chrome.busy;
    let entity = chrome.entity;
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_1()
        .child(inclusion_box(
            SharedString::from(format!("hunk-mark-{index}")),
            marked,
            format!("Include this hunk of {path} in the commit. Space toggles the focused hunk."),
            {
                let path = path.clone();
                let header = header.clone();
                let mark_entity = entity.clone();
                move |_: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    mark_entity.update(cx, |this, cx| {
                        this.store.pane = Pane::Changes;
                        this.store.set_cursor(Pane::Changes, index);
                        this.store.toggle_hunk_mark(&path, &header);
                        cx.notify();
                    });
                }
            },
        ))
        .when(busy, |row| {
            row.child(wait_slot(
                SharedString::from(format!("hunk-busy-{index}")),
                "This file is in progress.".to_string(),
            ))
        })
        .child(more_button(
            SharedString::from(format!("hunk-more-{index}")),
            move |event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                let position = event.position();
                let x = position.x.as_f32();
                let y = position.y.as_f32();
                entity.update(cx, |this, cx| {
                    this.store.pane = Pane::Changes;
                    this.store.set_cursor(Pane::Changes, index);
                    let effect = this.store.open_hunk_actions(&path, &header);
                    this.place_effect(effect, x, y, cx);
                });
            },
        ))
        .into_any_element()
}

fn inclusion_box(
    id: SharedString,
    on: bool,
    tip: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    div()
        .id(id)
        .w(px(16.))
        .h(px(16.))
        .flex_none()
        .rounded_sm()
        .border_1()
        .border_color(if on {
            gpui::Hsla::from(theme.accent)
        } else {
            gpui::Hsla::from(theme.border)
        })
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(theme.accent)
        .cursor_pointer()
        .when(on, |mark| mark.bg(theme.selected))
        .tooltip(Tooltip::text(tip))
        .on_click(on_click)
        .child(if on { "✓" } else { "" })
        .into_any_element()
}

fn more_button(
    id: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    div()
        .id(id)
        .w(px(22.))
        .h(px(22.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_sm()
        .cursor_pointer()
        .tooltip(Tooltip::text("More actions"))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .child("⋯")
        .into_any_element()
}

fn quiet_body() -> gpui::AnyElement {
    div().flex_1().min_h(px(0.)).into_any_element()
}

/// Acting sentence. Cancel and a percent come from the operation catalog.
fn pace_sentence(id: &str, lead: &str) -> String {
    let known = store::operation_spec(id);
    let cancel = known.is_some_and(|item| item.cancel);
    let indeterminate = known.is_some_and(|item| item.indeterminate);
    match (cancel, indeterminate) {
        (true, true) => format!("{lead} You can cancel it. This does not show a percent."),
        (true, false) => format!("{lead} You can cancel it."),
        (false, true) => format!("{lead} This cannot be cancelled."),
        (false, false) => lead.to_string(),
    }
}

fn word_button(
    id: SharedString,
    label: &'static str,
    tip: String,
    danger: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    div()
        .id(id)
        .h(px(20.))
        .min_w(px(36.))
        .px_1()
        .flex_none()
        .rounded_sm()
        .border_1()
        .border_color(if danger {
            gpui::Hsla::from(theme.err)
        } else {
            gpui::Hsla::from(theme.border)
        })
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .cursor_pointer()
        .text_color(if danger { theme.err } else { theme.text })
        .tooltip(Tooltip::text(tip))
        .on_click(on_click)
        .child(label)
        .into_any_element()
}

fn inert_word(id: SharedString, label: &'static str, tip: String) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    div()
        .id(id)
        .h(px(20.))
        .min_w(px(36.))
        .px_1()
        .flex_none()
        .rounded_sm()
        .border_1()
        .border_color(gpui::Hsla::from(theme.border))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(theme.muted)
        .tooltip(Tooltip::text(tip))
        .child(label)
        .into_any_element()
}

fn notice_line(id: &'static str, text: &str, bad: bool) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    let color = if text.is_empty() {
        theme.muted
    } else if bad {
        theme.err
    } else {
        theme.ok
    };
    div()
        .id(id)
        .flex_none()
        .h(px(20.))
        .px_3()
        .flex()
        .items_center()
        .overflow_hidden()
        .text_xs()
        .text_color(color)
        .when(!text.is_empty(), |row| {
            row.tooltip(Tooltip::text(text.to_string()))
        })
        .child(div().min_w(px(0.)).truncate().child(text.to_string()))
        .into_any_element()
}

fn wait_slot(id: SharedString, tip: String) -> gpui::AnyElement {
    div()
        .id(id)
        .h(px(20.))
        .min_w(px(16.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(ns_ui::theme().accent)
        .tooltip(Tooltip::text(tip))
        .child("…")
        .into_any_element()
}

fn same_commit(left: &str, right: &str) -> bool {
    left == right || short7(left) == short7(right)
}

fn gutter(line: Option<u32>) -> gpui::Div {
    let theme = ns_ui::theme();
    div()
        .w(px(36.))
        .flex_none()
        .text_right()
        .text_color(theme.muted)
        .child(line.map(|line| line.to_string()).unwrap_or_default())
}

fn status_mark(status: FileStatus) -> gpui::Div {
    let theme = ns_ui::theme();
    let color = match status {
        FileStatus::Added => theme.ok,
        FileStatus::Modified => theme.accent,
        FileStatus::Deleted => theme.err,
        FileStatus::Renamed => theme.warn,
    };
    div()
        .w(px(14.))
        .flex_none()
        .font_family(ns_ui::CODE_FONT)
        .text_xs()
        .text_color(color)
        .child(status.mark())
}

fn counts(additions: u32, deletions: u32) -> gpui::Div {
    let theme = ns_ui::theme();
    div()
        .flex_none()
        .flex()
        .gap_1()
        .text_xs()
        .font_family(ns_ui::CODE_FONT)
        .child(div().text_color(theme.ok).child(format!("+{additions}")))
        .child(div().text_color(theme.err).child(format!("-{deletions}")))
}

fn marker_chip(index: usize, label: &str, tip: &str, tone: Tone) -> impl IntoElement {
    div()
        .id(SharedString::from(format!("marker-{index}-{label}")))
        .flex_none()
        .tooltip(Tooltip::text(tip.to_string()))
        .child(ns_ui::chip(label.to_string(), tone))
}

fn target_line(target: &str) -> impl IntoElement {
    let theme = ns_ui::theme();
    let tip = if target == "No target branch" {
        "No remote is set. Choose a target branch before you fetch or push.".to_string()
    } else {
        format!("Target branch {target}")
    };
    div()
        .id("target-ref")
        .w_full()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_2()
        .px_1()
        .tooltip(Tooltip::text(tip))
        .child(
            div()
                .flex_none()
                .text_color(theme.muted)
                .text_xs()
                .child("Target"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .font_family(ns_ui::CODE_FONT)
                .text_xs()
                .truncate()
                .child(target.to_string()),
        )
}

fn meta_line(id: &str, label: &str, value: &str) -> impl IntoElement {
    meta_line_inner(id, label, value, false)
}

/// Same row as [`meta_line`], with the value wrapping instead of ending in an ellipsis.
fn meta_line_wrap(id: &str, label: &str, value: &str) -> impl IntoElement {
    meta_line_inner(id, label, value, true)
}

fn meta_line_inner(id: &str, label: &str, value: &str, wrap: bool) -> impl IntoElement {
    let theme = ns_ui::theme();
    div()
        .id(SharedString::from(id.to_string()))
        .w_full()
        .min_w(px(0.))
        .flex()
        .items_start()
        .gap_2()
        .text_xs()
        .when(!wrap, |row| row.overflow_hidden())
        .tooltip(Tooltip::text(format!("{label}: {value}")))
        .child(
            div()
                .w(px(72.))
                .flex_none()
                .text_color(theme.muted)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .font_family(ns_ui::CODE_FONT)
                .when(wrap, |value| value.whitespace_normal())
                .when(!wrap, |value| value.truncate())
                .child(value.to_string()),
        )
}

/// Drop blank lines so a subject and body stay two lines, not three.
/// The tooltip still carries the original message.
fn compact_message(message: &str) -> String {
    message
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn copy_button(id: &'static str, text: String, cx: &mut Context<GbWindow>) -> gpui::AnyElement {
    ns_ui::icon_button(
        id,
        "copy",
        "Copy the full SHA",
        ns_ui::IconTone::Quiet,
        cx.listener(move |_, _: &ClickEvent, _, _| {
            ns_ui::copy_text(&text);
        }),
    )
    .into_any_element()
}

fn wrapped_note(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .w_full()
        .min_w(px(0.))
        .px_3()
        .py_2()
        .whitespace_normal()
        .child(ns_ui::awareness(text))
}

fn app_settings_rows(
    global: &gb_bridge::GlobalSettings,
    cursor: usize,
    fetch_draft: &str,
    fetch_focused: bool,
    cx: &mut Context<GbWindow>,
) -> gpui::AnyElement {
    let fetch_value = if fetch_focused {
        format!("{fetch_draft}▌")
    } else {
        fetch_draft.to_string()
    };
    let mut rows = vec![
        settings_choice(
            "settings-metrics",
            "Share anonymous usage numbers",
            if global.metrics_enabled { "On" } else { "Off" },
            "Anonymous usage numbers. Off until you turn this on.",
            cursor == 0,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let effect = this.store.click_settings(0);
                this.activate(effect, cx);
                cx.notify();
            }),
        ),
        settings_choice(
            "settings-errors",
            "Send anonymous error reports",
            if global.error_reporting_enabled {
                "On"
            } else {
                "Off"
            },
            "Anonymous error reports. Off until you turn this on.",
            cursor == 1,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let effect = this.store.click_settings(1);
                this.activate(effect, cx);
                cx.notify();
            }),
        ),
        settings_choice(
            "settings-fetch",
            "Fetch interval (minutes)",
            &fetch_value,
            "How often to fetch. A negative number, such as -1, turns auto-fetch off. Enter edits the number.",
            cursor == 2,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let effect = this.store.click_settings(2);
                this.activate(effect, cx);
                cx.notify();
            }),
        ),
        settings_choice(
            "settings-title-bar",
            "Use the native title bar",
            if global.native_title_bar { "On" } else { "Off" },
            "Draw the system title bar. Takes effect the next time the window opens.",
            cursor == 3,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let effect = this.store.click_settings(3);
                this.activate(effect, cx);
                cx.notify();
            }),
        ),
        settings_choice(
            "settings-shadow",
            "Hide the window shadow",
            if global.hide_window_shadow {
                "On"
            } else {
                "Off"
            },
            "Draw the window without a drop shadow on macOS. Takes effect the next time the window opens.",
            cursor == 4,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let effect = this.store.click_settings(4);
                this.activate(effect, cx);
                cx.notify();
            }),
        ),
    ];
    rows.push(settings_note(
        "settings-scale",
        "This build has no scale or theme control. Appearance here is the native title bar and the window shadow.",
    ));
    rows.push(settings_note("settings-forge", gb_bridge::FORGE_SIGN_IN));
    for flag in &global.feature_flags {
        rows.push(settings_note(
            SharedString::from(format!("settings-flag-{}", flag.name)),
            &format!("{}: {}", flag.label, flag.value),
        ));
    }
    div()
        .w_full()
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .children(rows)
        .into_any_element()
}

fn project_settings_rows(
    project: Option<&gb_bridge::ProjectSettings>,
    error: Option<&str>,
    cursor: usize,
    cx: &mut Context<GbWindow>,
) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    if project.is_none() && error.is_none() {
        rows.push(settings_note(
            "settings-project-empty",
            "Open a project to see its target branch, signing, and removal.",
        ));
    }
    if let Some(error) = error {
        rows.push(
            div()
                .id("settings-project-error")
                .h(px(36.))
                .px_2()
                .flex()
                .items_center()
                .text_xs()
                .text_color(theme.err)
                .truncate()
                .tooltip(Tooltip::text(error.to_string()))
                .child(error.to_string())
                .into_any_element(),
        );
    }
    let target = project
        .and_then(|item| item.target_branch.clone())
        .unwrap_or_else(|| "No target branch yet.".to_string());
    let signing = project
        .map(|item| item.signing.summary.clone())
        .unwrap_or_else(|| "Commit signing is off.".to_string());
    let allowed = project.is_some_and(|item| item.remove_allowed);
    rows.push(settings_choice(
        "settings-target",
        "Target branch",
        &target,
        "The branch this workspace integrates into. Read-only here.",
        cursor == 0,
        cx.listener(|this, _: &ClickEvent, _, cx| {
            let effect = this.store.click_settings(0);
            this.activate(effect, cx);
            cx.notify();
        }),
    ));
    rows.push(settings_choice(
        "settings-signing",
        "Commit signing",
        &signing,
        "Read from git config. This does not use the keychain or sign a commit.",
        cursor == 1,
        cx.listener(|this, _: &ClickEvent, _, cx| {
            let effect = this.store.click_settings(1);
            this.activate(effect, cx);
            cx.notify();
        }),
    ));
    if allowed {
        rows.push(settings_choice(
            "settings-remove",
            "Remove project",
            "Ask first",
            "Remove this project from the app after a confirmation. The repository folder stays on disk.",
            cursor == 2,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                let effect = this.store.click_settings(2);
                this.activate(effect, cx);
                cx.notify();
            }),
        ));
    } else {
        rows.push(inert_word(
            SharedString::from("settings-remove"),
            "Remove project",
            "Removing a project is turned off unless the app data directory is a test directory."
                .to_string(),
        ));
    }
    div()
        .w_full()
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .children(rows)
        .into_any_element()
}

fn settings_choice(
    id: &'static str,
    label: &'static str,
    value: &str,
    tip: &str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    let value = value.to_string();
    div()
        .id(SharedString::from(id))
        .h(px(36.))
        .w_full()
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .overflow_hidden()
        .cursor_pointer()
        .border_1()
        .border_color(ring(selected, &theme))
        .when(selected, |row| row.bg(theme.selected))
        .tooltip(Tooltip::text(tip.to_string()))
        .on_click(on_click)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_sm()
                .truncate()
                .child(label),
        )
        .child(
            div()
                .id(SharedString::from(format!("{id}-value")))
                .flex_none()
                .max_w(px(280.))
                .font_family(ns_ui::CODE_FONT)
                .text_xs()
                .text_color(theme.muted)
                .truncate()
                .child(value),
        )
        .into_any_element()
}

fn settings_note(id: impl Into<SharedString>, text: &str) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    div()
        .id(id.into())
        .min_h(px(28.))
        .px_2()
        .flex()
        .items_center()
        .text_xs()
        .text_color(theme.muted)
        .tooltip(Tooltip::text(text.to_string()))
        .child(text.to_string())
        .into_any_element()
}

fn help_line(index: usize, keys: &str, what: &str, tokens: &[&str]) -> gpui::AnyElement {
    let theme = ns_ui::theme();
    let tip = if tokens.is_empty() {
        what.to_string()
    } else {
        format!("{what} ({})", tokens.join(", "))
    };
    div()
        .id(SharedString::from(format!("help-row-{index}")))
        .flex()
        .gap_3()
        .text_xs()
        .tooltip(Tooltip::text(tip))
        .child(
            div()
                .w(px(128.))
                .flex_none()
                .font_family(ns_ui::CODE_FONT)
                .child(keys.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(theme.muted)
                .whitespace_normal()
                .child(what.to_string()),
        )
        .into_any_element()
}

fn ring(on: bool, theme: &ns_ui::Theme) -> gpui::Hsla {
    if on {
        gpui::Hsla::from(theme.accent)
    } else {
        gpui::transparent_black()
    }
}

fn key_token(keystroke: &gpui::Keystroke) -> Option<String> {
    let key = keystroke.key.as_str();
    let mods = &keystroke.modifiers;
    if mods.platform && mods.shift && !mods.control && !mods.alt && key.eq_ignore_ascii_case("f") {
        return Some("cmd-shift-f".to_string());
    }
    if mods.platform && !mods.control && !mods.alt && !mods.shift {
        if key.eq_ignore_ascii_case("r") {
            return Some("cmd-r".to_string());
        }
        if key.eq_ignore_ascii_case("p") {
            return Some("cmd-p".to_string());
        }
        if key.eq_ignore_ascii_case("z") {
            return Some("cmd-z".to_string());
        }
        if key == "," || key.eq_ignore_ascii_case("comma") {
            return Some("cmd-comma".to_string());
        }
    }
    if mods.platform || mods.control || mods.alt || mods.function {
        return None;
    }
    if mods.shift && key == "tab" {
        return Some("shift-tab".to_string());
    }
    if mods.shift && key.chars().count() == 1 && !key.chars().any(char::is_control) {
        return Some(key.to_string());
    }
    if mods.shift {
        return None;
    }
    match key {
        "up" | "down" | "left" | "right" | "tab" | "enter" | "escape" | "backspace" | "space"
        | "j" | "k" | "t" | "m" | "d" | "c" | "r" | "s" | "u" | "i" | "a" | "x" => {
            Some(key.to_string())
        }
        "esc" => Some("escape".to_string()),
        " " => Some("space".to_string()),
        other if other.chars().count() == 1 && !other.chars().any(char::is_control) => {
            Some(other.to_string())
        }
        _ => None,
    }
}

fn project_button_label(store: &Store) -> String {
    let Some(id) = &store.selected else {
        return "Choose project".to_string();
    };
    let title = store
        .projects
        .iter()
        .find(|project| &project.id == id)
        .map(|project| project.title.as_str())
        .unwrap_or("Choose project");
    truncate_chars(title, 28)
}

fn project_button_tip(store: &Store) -> String {
    let Some(id) = &store.selected else {
        return "Choose a project (⌘P)".to_string();
    };
    match store.projects.iter().find(|project| &project.id == id) {
        Some(project) => format!("{} · {} · ⌘P", project.title, project.path),
        None => "Choose a project (⌘P)".to_string(),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn status_line(store: &Store, preview: bool) -> String {
    let when = store
        .last_refresh
        .map(clock)
        .unwrap_or_else(|| "--:--:--".to_string());
    if preview {
        return format!("Preview (fake data) · changes stay in memory · updated {when}");
    }
    let title = store
        .selected
        .as_ref()
        .and_then(|id| {
            store
                .projects
                .iter()
                .find(|project| &project.id == id)
                .map(|project| project.title.as_str())
        })
        .unwrap_or("project");
    format!("Live · writing through GitButler · watching {title} · updated {when}")
}

fn clock(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%H:%M:%S")
        .to_string()
}

fn clock_date(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown time".to_string())
}

fn relative(seconds: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(seconds);
    let delta = now.saturating_sub(seconds);
    if delta < 60 {
        "just now".to_string()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

fn dated(seconds: i64) -> String {
    format!("{} ({})", clock_date(seconds), relative(seconds))
}

fn date_range(earliest: Option<i64>, latest: Option<i64>) -> Option<String> {
    match (earliest, latest) {
        (Some(early), Some(late)) if early == late => Some(dated(early)),
        // One date per line so the second date does not break mid-phrase.
        (Some(early), Some(late)) => Some(format!("{}\n{}", dated(early), dated(late))),
        (Some(only), None) | (None, Some(only)) => Some(dated(only)),
        (None, None) => None,
    }
}

fn short7(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Directory the coordinator can check. A canned fake path such as
/// `/demo/app-data` is not a GitButler channel directory, so the reply then
/// uses the directory compiled into `but-path`. That lookup does not create it.
#[cfg(feature = "devtools")]
fn state_app_data_dir(store: &Store) -> String {
    if let Some(env) = &store.env {
        let path = &env.app_data_dir;
        if path.ends_with(gb_bridge::RELEASE_IDENTIFIER)
            || path.ends_with("com.gitbutler.app.dev")
            || path.ends_with("com.gitbutler.app.nightly")
        {
            return path.display().to_string();
        }
    }
    gb_bridge::compiled_app_data_dir().display().to_string()
}

#[cfg(feature = "devtools")]
fn hunk_header_arg(store: &Store, path: &str, hunk: &str) -> Result<String, String> {
    if hunk.starts_with("@@") {
        return Ok(hunk.to_string());
    }
    if let Ok(index) = hunk.parse::<usize>() {
        return store
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.files.iter().find(|file| file.path == path))
            .and_then(|file| file.hunks.get(index))
            .map(|hunk| hunk.header.clone())
            .ok_or_else(|| format!("no hunk {index} in {path}"));
    }
    Ok(hunk.to_string())
}

#[cfg(feature = "devtools")]
fn arg_bool(args: &serde_json::Value, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(serde_json::Value::Bool(value)) => Some(*value),
        Some(serde_json::Value::String(text)) => match text.as_str() {
            "true" | "on" | "1" => Some(true),
            "false" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(feature = "devtools")]
fn arg_f32(args: &serde_json::Value, key: &str) -> Option<f32> {
    match args.get(key) {
        Some(serde_json::Value::Number(number)) => number.as_f64().map(|value| value as f32),
        Some(serde_json::Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

#[cfg(feature = "devtools")]
fn arg_text(args: &serde_json::Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(value) if value.is_string() => value.as_str().map(str::to_string),
        Some(value) if value.is_number() => value
            .as_u64()
            .or_else(|| value.as_i64().map(|n| n as u64))
            .map(|n| n.to_string()),
        _ => None,
    }
}

#[cfg(feature = "devtools")]
fn resolve_commit(store: &Store, commit: &str) -> Result<String, String> {
    if commit.len() <= 4 && commit.chars().all(|ch| ch.is_ascii_digit()) {
        let index: usize = commit
            .parse()
            .map_err(|_| format!("bad commit index {commit}"))?;
        store
            .commit_sha_at(index)
            .ok_or_else(|| format!("no commit at index {index}"))
    } else {
        Ok(commit.to_string())
    }
}

#[cfg(feature = "devtools")]
fn menu_flag(args: &serde_json::Value) -> Result<bool, String> {
    if let Some(action) = arg_text(args, "action") {
        return match action.as_str() {
            "open" => Ok(true),
            "close" => Ok(false),
            other => Err(format!("project-menu action is open or close, got {other}")),
        };
    }
    if let Some(value) = args.get("open") {
        return match value {
            serde_json::Value::Bool(open) => Ok(*open),
            serde_json::Value::String(text) => Ok(text != "false" && text != "0"),
            _ => Err("project-menu open needs true or false".to_string()),
        };
    }
    if args.get("close").is_some() {
        return Ok(false);
    }
    Err("project-menu needs action=open or action=close".to_string())
}

/// `action=open` or a bare `open` key. ns-dev sends `open` with no value as null.
#[cfg(feature = "devtools")]
fn named_action(
    args: &serde_json::Value,
    command: &str,
    choices: &[&str],
) -> Result<String, String> {
    if let Some(action) = arg_text(args, "action") {
        if choices.iter().any(|choice| *choice == action) {
            return Ok(action);
        }
        return Err(format!(
            "{command} action is {}, got {action}",
            choices.join(", ")
        ));
    }
    let hits: Vec<&str> = choices
        .iter()
        .copied()
        .filter(|choice| args.get(*choice).is_some())
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err(format!("{command} needs action={}", choices.join("|"))),
        many => Err(format!(
            "{command} got more than one action: {}",
            many.join(", ")
        )),
    }
}

#[cfg(feature = "devtools")]
fn force_arg(args: &serde_json::Value) -> Result<ForcePush, String> {
    match arg_text(args, "force").as_deref() {
        None | Some("no") | Some("regular") => Ok(ForcePush::No),
        Some("lease") | Some("with-lease") => Ok(ForcePush::WithLease),
        Some("force") => Ok(ForcePush::Force),
        Some(other) => Err(format!("push force is lease or force, got {other}")),
    }
}

/// A present `confirm` key means the dev command already confirmed a force push.
#[cfg(feature = "devtools")]
fn confirmed(args: &serde_json::Value) -> bool {
    match args.get("confirm") {
        None => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::String(text)) => text != "false" && text != "0",
        Some(_) => true,
    }
}

#[cfg(feature = "devtools")]
impl ns_ui::DevControl for GbWindow {
    fn dev(
        &mut self,
        command: &str,
        args: &serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, String> {
        let compat = self.store.env.as_ref().map(|env| match &env.compat {
            gb_bridge::Compat::Match => "match",
            gb_bridge::Compat::Mismatch { .. } => "mismatch",
            gb_bridge::Compat::NotInstalled => "not_installed",
        });
        match command {
            "state" => {
                let stacks: Vec<&str> = self
                    .store
                    .workspace
                    .as_ref()
                    .map(|workspace| {
                        workspace
                            .stacks
                            .iter()
                            .filter_map(|stack| {
                                stack.branches.first().map(|branch| branch.name.as_str())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut state = serde_json::json!({
                    "compat": compat,
                    "projects": self.store.projects.len(),
                    "selected": self.store.selected,
                    "stacks": stacks,
                    "last_refresh": self.store.last_refresh.map(clock),
                    "reason": self.store.list_reason,
                    "app_data_dir": state_app_data_dir(&self.store),
                    "watcher": if self.store.watching { "running" } else { "stopped" },
                    "exclusive": self.store.exclusive,
                    "changes": self.store.change_count,
                    "pane": self.store.pane.name(),
                    "selected_item": self.store.selected_item(),
                    "columns": store::COLUMNS,
                    "view": self.store.view.name(),
                    "project_menu": self.store.project_menu,
                    "focused": self.store.focused_kind().name(),
                    "detail_pane": self.store.detail_open(),
                    "changes_heading": self.store.changes_heading(),
                    "diff_ready": self.store.diff_ready(),
                    "commit_ready": self.store.commit_ready(),
                    "screen": self.store.pane.name(),
                    "selection": self.store.selected_item(),
                    "busy": self.store.busy.is_some()
                        || self.store.loading
                        || self.store.refreshing
                        || self.store.diff_loading
                        || self.store.commit_loading,
                    "composer": if self.store.composer.open { "open" } else { "closed" },
                    "composer_lane": self.store.composer_lane_label(),
                    "composer_title": self.store.composer.title,
                    "composer_body": self.store.composer.body,
                    "hook": self.store.composer.hook,
                    "busy_op": self.store.busy.as_ref().map(|busy| busy.op.clone()),
                    "busy_row": self.store.busy.as_ref().and_then(|busy| busy.row.clone()),
                    "undo": self.store.undo_caption(),
                    "move_menu": self.store.menu.as_ref().map(|_| self.store.menu_title()),
                    "confirm": self.store.confirm.as_ref().map(|confirm| confirm.title.clone()),
                    "marked": self.store.marked.len(),
                    "column_widths": self.laid_out,
                    "column_fractions": self.store.column_fractions,
                });
                // A second object: one giant `json!` exceeds the macro recursion limit.
                // Flags only. The askpass draft is not a field.
                let remote = serde_json::json!({
                    "needs_setup": self.store.needs_setup(),
                    "setup_branches": self.store.remote.branches.len(),
                    "fetching": self.store.remote.fetching,
                    "fetch_summary": self.store.remote.status_line(),
                    "upstream": self.store.remote.upstream_count,
                    "integrate_open": self.store.remote.integrate_open,
                    "preview_busy": self.store.remote.preview_busy,
                    "integrate_busy": !self.store.remote.integrate_busy.is_empty(),
                    "push_menu": self.store.remote.push_menu,
                    "push_busy": self.store.remote.push_busy,
                    "push_confirm": self.store.remote.push_confirm.is_some(),
                    "askpass": self.store.remote.askpass.is_some(),
                });
                if let (Some(state), Some(remote)) = (state.as_object_mut(), remote.as_object()) {
                    for (key, value) in remote {
                        state.insert(key.clone(), value.clone());
                    }
                }
                let pages = page_flags(&self.store);
                if let (Some(state), Some(pages)) = (state.as_object_mut(), pages.as_object()) {
                    for (key, value) in pages {
                        state.insert(key.clone(), value.clone());
                    }
                }
                let forge = forge_flags(&self.store);
                if let (Some(state), Some(forge)) = (state.as_object_mut(), forge.as_object()) {
                    for (key, value) in forge {
                        state.insert(key.clone(), value.clone());
                    }
                }
                let settings = settings_flags(&self.store);
                if let (Some(state), Some(settings)) = (state.as_object_mut(), settings.as_object())
                {
                    for (key, value) in settings {
                        state.insert(key.clone(), value.clone());
                    }
                }
                let toast_messages: Vec<String> = self
                    .store
                    .activity
                    .visible_toasts()
                    .into_iter()
                    .map(|toast| toast.message)
                    .collect();
                let activity = serde_json::json!({
                    "activity_count": self.store.activity.activity_count(),
                    "activity_line": self.store.activity.strip_line(),
                    "latency_ms": self.backend.preview_latency_ms(),
                    "loading_ops": self.store.activity.loading_ids(),
                    "toasts": toast_messages,
                    "strip": self.store.strip_text(),
                });
                if let (Some(state), Some(activity)) = (state.as_object_mut(), activity.as_object())
                {
                    for (key, value) in activity {
                        state.insert(key.clone(), value.clone());
                    }
                }
                Ok(state)
            }
            "latency" => {
                self.require_preview()?;
                let ms = args
                    .get("ms")
                    .and_then(|value| value.as_u64())
                    .ok_or("latency needs ms=")?;
                self.backend.set_preview_latency_ms(ms);
                cx.notify();
                Ok(serde_json::json!({ "latency_ms": ms }))
            }
            "fail" => {
                self.require_preview()?;
                let op = arg_text(args, "op").ok_or("fail needs op=")?;
                if store::operation_spec(&op).is_none() {
                    return Err(format!("unknown operation {op}"));
                }
                self.backend.arm_preview_fail(&op);
                Ok(serde_json::json!({ "fail": op }))
            }
            "select" => {
                let id = if let Some(index) = args.get("index").and_then(|value| value.as_u64()) {
                    self.store
                        .projects
                        .get(index as usize)
                        .map(|project| project.id.clone())
                        .ok_or_else(|| format!("no project at index {index}"))?
                } else if let Some(id) = arg_text(args, "id") {
                    id
                } else {
                    return Err("select needs index or id".to_string());
                };
                self.select_project(id, cx);
                cx.notify();
                Ok(serde_json::json!({ "selected": self.store.selected }))
            }
            "refresh" => {
                self.refresh(cx);
                cx.notify();
                Ok(serde_json::json!({ "ok": true }))
            }
            "project-menu" => {
                let open = menu_flag(args)?;
                self.store.set_project_menu(open);
                cx.notify();
                Ok(serde_json::json!({ "project_menu": self.store.project_menu }))
            }
            "view" => {
                let mode = arg_text(args, "mode").ok_or("view needs mode=list or mode=tree")?;
                let mode = ViewMode::from_name(&mode).ok_or("view mode is list or tree")?;
                self.store.set_view(mode);
                cx.notify();
                Ok(serde_json::json!({ "view": self.store.view.name() }))
            }
            "focus" => {
                if let Some(name) = arg_text(args, "branch") {
                    if !self.store.focus_branch(&name) {
                        return Err(format!("no branch {name}"));
                    }
                    cx.notify();
                    Ok(serde_json::json!({ "branch": name, "detail_pane": true }))
                } else if let Some(commit) = arg_text(args, "commit") {
                    let sha = resolve_commit(&self.store, &commit)?;
                    let effect = self.store.focus_commit(&sha);
                    let reported = match &effect {
                        Effect::ShowCommit(full) => full.clone(),
                        _ => sha,
                    };
                    self.activate(effect, cx);
                    cx.notify();
                    Ok(serde_json::json!({ "commit": reported }))
                } else {
                    Err("focus needs branch or commit".to_string())
                }
            }
            "pane" => {
                let name = arg_text(args, "name").ok_or("pane needs name")?;
                if name == "projects" {
                    self.store.set_project_menu(true);
                    cx.notify();
                    return Ok(serde_json::json!({ "project_menu": true }));
                }
                let pane = Pane::from_name(&name)
                    .ok_or("pane name is uncommitted, changes, branches, or detail")?;
                if !self.store.focus_pane(pane) {
                    return Err("the detail pane is closed".to_string());
                }
                cx.notify();
                Ok(serde_json::json!({ "pane": pane.name() }))
            }
            "open" => {
                if let Some(commit) = arg_text(args, "commit") {
                    let sha = resolve_commit(&self.store, &commit)?;
                    let effect = self.store.focus_commit(&sha);
                    let reported = match &effect {
                        Effect::ShowCommit(full) => full.clone(),
                        _ => sha,
                    };
                    self.activate(effect, cx);
                    cx.notify();
                    Ok(serde_json::json!({ "commit": reported }))
                } else if let Some(path) = arg_text(args, "file") {
                    let request = self
                        .store
                        .focus_file(&path)
                        .ok_or_else(|| format!("no file {path}"))?;
                    self.load_diff(request, cx);
                    cx.notify();
                    Ok(serde_json::json!({ "file": path }))
                } else {
                    Err("open needs commit or file".to_string())
                }
            }
            "key" => {
                // Apply the key in the store so a hidden window still moves.
                // `Keystroke::parse` rejects strings the real keyboard would not send.
                let key = arg_text(args, "key").ok_or("key needs key=up, escape, cmd-r, …")?;
                let normalized = if key == "esc" {
                    "escape".to_string()
                } else {
                    key
                };
                gpui::Keystroke::parse(&normalized).map_err(|error| error.to_string())?;
                let effect = self.store.apply_key(&normalized);
                self.activate(effect, cx);
                self.scroll_diff();
                if self.store.branch_filter_pending() {
                    self.schedule_branch_refilter(cx);
                }
                cx.notify();
                Ok(serde_json::json!({ "key": normalized }))
            }
            "trigger-event" => {
                if !self.backend.is_preview() {
                    return Err("trigger-event works on the preview backend only".to_string());
                }
                let kind = arg_text(args, "kind").unwrap_or_else(|| "worktree".to_string());
                let id = self
                    .store
                    .selected
                    .clone()
                    .ok_or("no project is selected")?;
                self.backend
                    .trigger_fake(&id, &kind)
                    .map_err(|error| error.message)?;
                cx.notify();
                Ok(serde_json::json!({ "ok": true, "kind": kind }))
            }
            "assign" => {
                self.require_preview()?;
                let path = arg_text(args, "path").ok_or("assign needs path")?;
                let lane = arg_text(args, "lane");
                let effect = self
                    .store
                    .assign_paths(std::slice::from_ref(&path), lane.as_deref())?;
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({ "ok": true, "undo": self.store.undo_caption() }))
            }
            "commit" => {
                self.require_preview()?;
                let title = arg_text(args, "title").ok_or("commit needs title")?;
                let body = arg_text(args, "body").unwrap_or_default();
                let lane = arg_text(args, "lane");
                let effect = self.store.commit_with(&title, &body, lane.as_deref())?;
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({ "ok": true, "undo": self.store.undo_caption() }))
            }
            "reword" => {
                self.require_preview()?;
                let commit = arg_text(args, "commit").ok_or("reword needs commit")?;
                let message = arg_text(args, "message").ok_or("reword needs message")?;
                let sha = resolve_commit(&self.store, &commit)?;
                let effect = self.store.reword_to(&sha, &message)?;
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({ "ok": true, "undo": self.store.undo_caption() }))
            }
            "squash" => {
                self.require_preview()?;
                let effect = if let (Some(subject), Some(target)) =
                    (arg_text(args, "subject"), arg_text(args, "target"))
                {
                    let subject = resolve_commit(&self.store, &subject)?;
                    let target = resolve_commit(&self.store, &target)?;
                    self.store.squash_pair(&subject, &target)?
                } else {
                    self.store.plan_squash()?
                };
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({ "ok": true, "undo": self.store.undo_caption() }))
            }
            "uncommit" => {
                self.require_preview()?;
                let effect = if let Some(commit) = arg_text(args, "commit") {
                    let sha = resolve_commit(&self.store, &commit)?;
                    self.store.uncommit_sha(&sha)?
                } else {
                    self.store.plan_uncommit()?
                };
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({ "ok": true, "undo": self.store.undo_caption() }))
            }
            "undo" => {
                self.require_preview()?;
                let effect = self.store.request_undo();
                if matches!(effect, Effect::None) {
                    return Err(self
                        .store
                        .toast
                        .clone()
                        .unwrap_or_else(|| "Nothing to undo.".to_string()));
                }
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({ "ok": true, "undo": self.store.undo_caption() }))
            }
            "composer" => {
                let open = if args.get("close").is_some() {
                    false
                } else if args.get("open").is_some() {
                    true
                } else if let Some(action) = arg_text(args, "action") {
                    match action.as_str() {
                        "open" => true,
                        "close" => false,
                        other => {
                            return Err(format!("composer action is open or close, got {other}"));
                        }
                    }
                } else {
                    return Err("composer needs open or close".to_string());
                };
                if open {
                    self.store.menu = None;
                    self.store.confirm = None;
                }
                self.store.set_composer_open(open);
                if let Some(title) = arg_text(args, "title") {
                    self.store.composer.title = title;
                }
                if let Some(body) = arg_text(args, "body") {
                    self.store.composer.body = body;
                }
                if let Some(lane) = arg_text(args, "lane") {
                    if lane == "new" {
                        self.store.composer.new_lane = true;
                        self.store.composer.field = Some(Field::LaneName);
                    } else {
                        self.store.composer.new_lane = false;
                        self.store.composer.lane = Some(lane);
                    }
                }
                cx.notify();
                Ok(serde_json::json!({
                    "composer": if self.store.composer.open { "open" } else { "closed" }
                }))
            }
            "columns" => {
                if args.get("reset").is_some() {
                    self.store.column_fractions = store::COLUMN_DEFAULT;
                } else {
                    let handle = arg_text(args, "handle")
                        .or_else(|| arg_text(args, "index"))
                        .ok_or("columns needs handle=0|1 and delta=, or reset")?;
                    let handle: usize = handle
                        .parse()
                        .map_err(|_| "columns handle is 0 or 1".to_string())?;
                    if handle > 1 {
                        return Err("columns handle is 0 or 1".to_string());
                    }
                    if let Some(width) = arg_f32(args, "width") {
                        self.viewport_width = width;
                    }
                    let delta = arg_f32(args, "delta").unwrap_or(0.0);
                    self.store.column_fractions = store::drag_handle(
                        self.store.column_fractions,
                        handle,
                        delta,
                        self.viewport_width,
                    );
                }
                self.persist_columns();
                self.laid_out =
                    store::column_pixels(self.store.column_fractions, self.viewport_width);
                cx.notify();
                Ok(serde_json::json!({
                    "column_widths": self.laid_out,
                    "fractions": self.store.column_fractions,
                }))
            }
            "page" => {
                let name = arg_text(args, "name")
                    .or_else(|| arg_text(args, "page"))
                    .ok_or("page needs name=workspace|branches|history")?;
                let page = AppPage::from_name(&name)
                    .ok_or("page name is workspace, branches, or history")?;
                let before = self.store.app_page();
                self.capture_scroll();
                let effect = self.store.show_page(page);
                if self.store.app_page() != before {
                    self.restore_scroll();
                    self.shown_page = self.store.app_page();
                }
                self.activate(effect, cx);
                cx.notify();
                Ok(page_flags(&self.store))
            }
            "menu" => {
                if args.get("close").is_some()
                    || arg_text(args, "action").as_deref() == Some("close")
                {
                    self.store.close_menu();
                    cx.notify();
                    return Ok(serde_json::json!({ "move_menu": null }));
                }
                self.store.set_project_menu(false);
                if let Some(file) = arg_text(args, "file") {
                    if let Some(hunk) = arg_text(args, "hunk") {
                        let header = hunk_header_arg(&self.store, &file, &hunk)?;
                        let _ = self.store.open_hunk_actions(&file, &header);
                    } else {
                        let _ = self.store.open_file_actions(&file);
                    }
                } else if let Some(commit) = arg_text(args, "commit") {
                    let sha = resolve_commit(&self.store, &commit)?;
                    self.store.open_actions_for(&sha)?;
                } else if let Some(lane) = arg_text(args, "lane") {
                    self.store.open_lane_actions(&lane)?;
                } else if let Some(branch) = arg_text(args, "branch") {
                    self.store.open_listed_branch(&branch);
                } else {
                    return Err("menu needs file, hunk, commit, lane, or branch".to_string());
                }
                if let (Some(x), Some(y)) = (arg_f32(args, "x"), arg_f32(args, "y")) {
                    self.store.place_menu(x, y);
                }
                cx.notify();
                Ok(serde_json::json!({
                    "move_menu": self.store.menu_title(),
                    "menu_place": self.store.menu_place(),
                }))
            }
            "add" => {
                self.require_preview()?;
                let path = arg_text(args, "path").ok_or("add needs path=")?;
                let effect = self.store.request_add(path);
                if matches!(effect, Effect::None) {
                    return Err(self
                        .store
                        .remote
                        .setup_error
                        .clone()
                        .or_else(|| self.store.toast.clone())
                        .unwrap_or_else(|| "Could not add that repository.".to_string()));
                }
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({
                    "selected": self.store.selected,
                    "projects": self.store.projects.len(),
                    "needs_setup": self.store.needs_setup(),
                }))
            }
            "setup" => {
                self.require_preview()?;
                let target = arg_text(args, "target").ok_or("setup needs target=")?;
                let push = arg_text(args, "remote").or_else(|| arg_text(args, "push"));
                let effect = self.store.request_set_target(target, push)?;
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({
                    "needs_setup": self.store.needs_setup(),
                    "setup_branches": self.store.remote.branches.len(),
                }))
            }
            "fetch" => {
                self.require_preview()?;
                let effect = self.store.request_fetch();
                if matches!(effect, Effect::None) && !self.store.remote.fetching {
                    return Err(self
                        .store
                        .toast
                        .clone()
                        .unwrap_or_else(|| "Fetch did not start.".to_string()));
                }
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({
                    "fetching": self.store.remote.fetching,
                    "fetch_summary": self.store.remote.status_line(),
                    "upstream": self.store.remote.upstream_count,
                }))
            }
            "integrate" => {
                self.require_preview()?;
                let action = named_action(args, "integrate", &["open", "apply"])?;
                let effect = match action.as_str() {
                    "apply" => self.store.confirm_integrate(),
                    _ => self.store.open_integrate(),
                };
                if matches!(effect, Effect::None) {
                    return Err(self
                        .store
                        .remote
                        .integrate_error
                        .clone()
                        .or_else(|| self.store.toast.clone())
                        .unwrap_or_else(|| "Nothing to integrate.".to_string()));
                }
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({
                    "integrate_open": self.store.remote.integrate_open,
                    "preview_busy": self.store.remote.preview_busy,
                    "upstream": self.store.remote.upstream_count,
                }))
            }
            "push" => {
                self.require_preview()?;
                let lane = arg_text(args, "lane").ok_or("push needs lane=")?;
                if let Some(menu) = args.get("menu") {
                    let open = match menu {
                        serde_json::Value::Bool(value) => *value,
                        serde_json::Value::String(text) => {
                            text != "close" && text != "false" && text != "0"
                        }
                        _ => true,
                    };
                    if open {
                        self.store.open_push_menu(&lane)?;
                    } else {
                        self.store.remote.push_menu = None;
                    }
                    cx.notify();
                    return Ok(serde_json::json!({ "push_menu": self.store.remote.push_menu }));
                }
                let force = force_arg(args)?;
                if !matches!(force, ForcePush::No) && !confirmed(args) {
                    self.store.ask_force_push(&lane, force)?;
                    cx.notify();
                    return Ok(serde_json::json!({ "push_confirm": true, "lane": lane }));
                }
                let effect = self.store.push_now(&lane, force)?;
                self.activate(effect, cx);
                cx.notify();
                Ok(serde_json::json!({
                    "lane": lane,
                    "result": self.store.remote.push_results.get(&lane),
                }))
            }
            "askpass" => {
                self.require_preview()?;
                let action = named_action(args, "askpass", &["trigger", "answer", "cancel"])?;
                match action.as_str() {
                    "trigger" => {
                        let id = self
                            .store
                            .selected
                            .clone()
                            .ok_or_else(|| "Choose a project first.".to_string())?;
                        self.backend
                            .trigger_fake(&id, "askpass")
                            .map_err(|error| error.message)?;
                        let pending = self
                            .backend
                            .askpass_pending()
                            .map_err(|error| error.message)?;
                        let request = pending
                            .last()
                            .cloned()
                            .ok_or_else(|| "The sign-in prompt did not open.".to_string())?;
                        self.store.open_askpass(request);
                        cx.notify();
                        Ok(serde_json::json!({ "askpass": true }))
                    }
                    "answer" => {
                        let text = arg_text(args, "text").unwrap_or_default();
                        self.store.set_askpass_text(&text);
                        let effect = self.store.submit_askpass();
                        self.activate(effect, cx);
                        cx.notify();
                        Ok(serde_json::json!({ "askpass": self.store.remote.askpass.is_some() }))
                    }
                    "cancel" => {
                        let effect = self.store.cancel_askpass();
                        self.activate(effect, cx);
                        cx.notify();
                        Ok(serde_json::json!({ "askpass": false }))
                    }
                    _ => Err("askpass action is trigger, answer, or cancel".to_string()),
                }
            }
            "branches" => {
                self.require_preview()?;
                let action = named_action(
                    args,
                    "branches",
                    &["open", "filter", "apply", "unapply", "delete"],
                )?;
                match action.as_str() {
                    "open" => {
                        if let Some(name) = arg_text(args, "tab") {
                            let tab = BranchTab::from_name(&name)
                                .ok_or("branches tab is local, remote, or pr")?;
                            self.store.set_branch_tab(tab);
                        }
                        let effect = if self.store.pages.branches_open {
                            Effect::None
                        } else {
                            self.store.open_branches()
                        };
                        run_dev_effect(self, effect, cx)
                    }
                    "filter" => {
                        if let Some(name) = arg_text(args, "tab") {
                            let tab = BranchTab::from_name(&name)
                                .ok_or("branches tab is local, remote, or pr")?;
                            self.store.set_branch_tab(tab);
                        }
                        let text = arg_text(args, "text")
                            .or_else(|| arg_text(args, "filter"))
                            .filter(|text| text != "true" && text != "false")
                            .unwrap_or_default();
                        let need_list = self.store.pages.branch_rows.is_empty();
                        self.store.set_branch_filter(text);
                        if need_list {
                            let effect = self.store.open_branches();
                            run_dev_effect(self, effect, cx)
                        } else {
                            cx.notify();
                            Ok(page_flags(&self.store))
                        }
                    }
                    "apply" => {
                        let name = arg_text(args, "name").ok_or("branches apply needs name=")?;
                        let effect = self.store.apply_named(&name)?;
                        run_dev_effect(self, effect, cx)
                    }
                    "unapply" => {
                        let name = arg_text(args, "name");
                        let effect = self.store.unapply_named(name.as_deref())?;
                        run_dev_effect(self, effect, cx)
                    }
                    "delete" => {
                        let name = arg_text(args, "name");
                        let effect = self.store.delete_named(name.as_deref())?;
                        run_dev_effect(self, effect, cx)
                    }
                    _ => Err("branches action is open, filter, apply, unapply, or delete".into()),
                }
            }
            "history" => {
                self.require_preview()?;
                let action =
                    named_action(args, "history", &["open", "select", "restore", "snapshot"])?;
                match action.as_str() {
                    "open" => {
                        let effect = if self.store.pages.history_open {
                            Effect::None
                        } else {
                            self.store.open_history()
                        };
                        run_dev_effect(self, effect, cx)
                    }
                    "select" => {
                        let id = arg_text(args, "id").ok_or("history select needs id=")?;
                        let effect = self.store.select_snapshot_id(&id)?;
                        run_dev_effect(self, effect, cx)
                    }
                    "restore" => {
                        let effect = self.store.restore_selected()?;
                        run_dev_effect(self, effect, cx)
                    }
                    "snapshot" => {
                        let message = arg_text(args, "message").unwrap_or_default();
                        self.store.set_snapshot_message(message.clone());
                        let effect = self.store.create_snapshot_now(&message)?;
                        run_dev_effect(self, effect, cx)
                    }
                    _ => Err("history action is open, select, restore, or snapshot".into()),
                }
            }
            "edit" => {
                self.require_preview()?;
                let action = named_action(args, "edit", &["commit", "save", "abort"])?;
                let effect = match action.as_str() {
                    "commit" => {
                        let sha = arg_text(args, "commit").ok_or("edit commit needs commit=")?;
                        self.store.request_enter_edit(&sha)?
                    }
                    "save" => self.store.request_save_edit()?,
                    "abort" => self.store.request_abort_edit()?,
                    _ => return Err("edit action is commit, save, or abort".to_string()),
                };
                run_dev_effect(self, effect, cx)
            }
            "resolve" => {
                self.require_preview()?;
                self.store.toast = None;
                self.store.toast_bad = false;
                let mark = args.get("mark").is_some()
                    || args.get("confirm").is_some()
                    || arg_text(args, "action").as_deref() == Some("mark");
                if let (Some(file), Some(side)) = (arg_text(args, "file"), arg_text(args, "side")) {
                    if !self.store.pages.conflicts_open
                        || self.store.pages.conflict_files.is_empty()
                    {
                        let effect = self.store.open_conflicts(None)?;
                        self.activate(effect, cx);
                        if self.store.toast_bad {
                            return Err(self
                                .store
                                .toast
                                .clone()
                                .unwrap_or_else(|| "The action failed.".to_string()));
                        }
                    }
                    self.store.choose_side(&file, &side)?;
                } else if !mark {
                    return Err("resolve needs file= and side=ours|theirs".to_string());
                }
                if mark {
                    let effect = self.store.mark_resolved()?;
                    return run_dev_effect(self, effect, cx);
                }
                cx.notify();
                if self.store.toast_bad {
                    return Err(self
                        .store
                        .toast
                        .clone()
                        .unwrap_or_else(|| "The action failed.".to_string()));
                }
                Ok(page_flags(&self.store))
            }
            "forge" => {
                self.require_preview()?;
                let action = named_action(args, "forge", &["publish"])?;
                match action.as_str() {
                    "publish" => {
                        let branch =
                            arg_text(args, "branch").ok_or("forge publish needs branch=")?;
                        let effect = self.store.request_publish(&branch)?;
                        run_dev_effect(self, effect, cx)
                    }
                    _ => Err("forge action is publish".to_string()),
                }
            }
            "suggest" => {
                self.require_preview()?;
                if args.get("replace").is_some()
                    || arg_text(args, "action").as_deref() == Some("replace")
                {
                    self.store.replace_held_message();
                    cx.notify();
                    return Ok(suggestion_flags(&self.store));
                }
                if args.get("keep").is_some() || arg_text(args, "action").as_deref() == Some("keep")
                {
                    self.store.keep_typed_message();
                    cx.notify();
                    return Ok(suggestion_flags(&self.store));
                }
                let effect = self.store.request_suggestion();
                let value = run_dev_effect(self, effect, cx)?;
                let mut value = value;
                if let (Some(object), Some(extra)) = (
                    value.as_object_mut(),
                    suggestion_flags(&self.store).as_object(),
                ) {
                    for (key, item) in extra {
                        object.insert(key.clone(), item.clone());
                    }
                }
                Ok(value)
            }
            "settings" => {
                self.require_preview()?;
                let action = named_action(
                    args,
                    "settings",
                    &["open", "close", "app", "project", "save"],
                )?;
                match action.as_str() {
                    "close" => {
                        self.store.close_settings();
                        cx.notify();
                        Ok(settings_flags(&self.store))
                    }
                    "open" | "app" | "project" => {
                        let section = if action == "project" {
                            store::SettingsSection::Project
                        } else {
                            store::SettingsSection::App
                        };
                        if !self.store.settings.open {
                            let effect = self.store.open_settings();
                            self.store.settings.section = section;
                            run_dev_effect(self, effect, cx)?;
                        } else {
                            self.store.show_settings_section(section);
                            cx.notify();
                        }
                        Ok(settings_flags(&self.store))
                    }
                    "save" => {
                        let patch = gb_bridge::GlobalPatch {
                            metrics_enabled: arg_bool(args, "metrics"),
                            error_reporting_enabled: arg_bool(args, "errors"),
                            fetch_interval_minutes: args.get("fetch").and_then(|value| {
                                value
                                    .as_i64()
                                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                            }),
                            native_title_bar: arg_bool(args, "title"),
                            hide_window_shadow: arg_bool(args, "shadow"),
                        };
                        if patch == gb_bridge::GlobalPatch::default() {
                            return Ok(settings_flags(&self.store));
                        }
                        let effect = store::Effect::Settings(store::SettingsOp::Save(patch));
                        run_dev_effect(self, effect, cx)?;
                        Ok(settings_flags(&self.store))
                    }
                    _ => Err("settings action is open, close, app, project, or save".to_string()),
                }
            }
            // `cmd` "help" is the shared socket command list and never reaches
            // this match. "keyboard" opens the shortcut card.
            "perf" => {
                self.require_preview()?;
                if arg_text(args, "action").as_deref() == Some("status") {
                    return Ok(self.perf_status());
                }
                if self.perf.borrow().recording {
                    return Ok(self.perf_status());
                }
                if self.store.loading || self.store.selected.is_none() {
                    return Err("perf waits until the project is open".to_string());
                }
                let label = arg_text(args, "label").unwrap_or_else(|| "after".to_string());
                if label != "before" && label != "after" {
                    return Err("perf label is before or after".to_string());
                }
                let scenario = arg_text(args, "scenario").unwrap_or_else(|| "all".to_string());
                let names = perf_harness::perf_names(&scenario)?;
                let full = label == "before" || args.get("full").is_some();
                self.start_perf(&label, names, full, cx);
                let path = perf_harness::output_path(&label);
                Ok(serde_json::json!({
                    "started": true,
                    "label": label,
                    "full": self.perf_full,
                    "path": path.display().to_string(),
                }))
            }
            "keyboard" => {
                let action = named_action(args, "keyboard", &["open", "close"])?;
                if action == "open" {
                    if !self.store.help {
                        self.store.toggle_help();
                    }
                } else {
                    self.store.help = false;
                }
                cx.notify();
                Ok(serde_json::json!({ "help": self.store.help }))
            }
            other => Err(format!("unknown command {other}")),
        }
    }
}

#[cfg(feature = "devtools")]
fn run_dev_effect(
    window: &mut GbWindow,
    effect: Effect,
    cx: &mut Context<GbWindow>,
) -> Result<serde_json::Value, String> {
    window.store.toast = None;
    window.store.toast_bad = false;
    window.activate(effect, cx);
    cx.notify();
    if window.store.toast_bad {
        return Err(window
            .store
            .toast
            .clone()
            .unwrap_or_else(|| "The action failed.".to_string()));
    }
    Ok(page_flags(&window.store))
}

#[cfg(feature = "devtools")]
fn forge_flags(store: &Store) -> serde_json::Value {
    let reviews: Vec<serde_json::Value> = store
        .forge
        .reviews
        .iter()
        .map(|review| {
            serde_json::json!({
                "number": review.number,
                "branch": review.branch,
                "state": review.state.label(),
                "ci": review.ci.label(),
                "draft": review.draft,
            })
        })
        .collect();
    serde_json::json!({
        "forge_connected": store.forge.connected,
        "publishing": store.forge.publishing,
        "review_count": reviews.len(),
        "reviews": reviews,
        "pr_selected": store.selected_review().map(|review| review.number),
        "suggest_busy": store.suggest_busy,
        "suggest_error": store.composer.suggest_error,
        "message_hold": store.message_hold.is_some(),
    })
}

#[cfg(feature = "devtools")]
fn suggestion_flags(store: &Store) -> serde_json::Value {
    serde_json::json!({
        "composer_title": store.composer.title,
        "composer_body": store.composer.body,
        "suggest_error": store.composer.suggest_error,
        "suggest_busy": store.suggest_busy,
        "message_hold": store.message_hold.is_some(),
    })
}

#[cfg(feature = "devtools")]
fn settings_flags(store: &Store) -> serde_json::Value {
    let global = store.settings.global.as_ref();
    let project = store.settings.project.as_ref();
    serde_json::json!({
        "settings_open": store.settings.open,
        "settings_section": store.settings.section.name(),
        "metrics": global.map(|item| item.metrics_enabled),
        "error_reporting": global.map(|item| item.error_reporting_enabled),
        "fetch_interval": global.map(|item| item.fetch_interval_minutes),
        "remove_allowed": project.map(|item| item.remove_allowed),
        "signing": project.map(|item| item.signing.summary.clone()),
        "settings_loading": store.settings.loading,
        "settings_error": store.settings.error,
        "help": store.help,
    })
}

#[cfg(feature = "devtools")]
fn page_flags(store: &Store) -> serde_json::Value {
    let file = store.pages.conflict_file();
    serde_json::json!({
        "page": store.app_page().name(),
        "branches_open": store.pages.branches_open,
        "branch_scroll": store.pages.branch_scroll,
        "history_scroll": store.pages.history_scroll,
        "menu_open": store.menu.is_some(),
        "branch_tab": store.pages.tab.name(),
        "branch_filter": store.pages.filter,
        "branch_selected": store.pages.selected_branch().map(|row| row.name.clone()),
        "history_open": store.pages.history_open,
        "snapshot": store.pages.snapshot_sha.clone(),
        "editing": store.pages.editing(),
        "edit_sha": store.pages.edit_sha.clone(),
        "conflicts_open": store.pages.conflicts_open,
        "conflict_file": file.map(|file| file.path.clone()),
        "conflict_side": file.and_then(|file| file.side.map(store::side_name)),
    })
}
