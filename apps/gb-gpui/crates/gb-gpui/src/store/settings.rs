//! Settings overlay state. The view paints it. The bridge reads and writes.

use gb_bridge::{Backend, GlobalPatch, GlobalSettings, ProjectSettings};

use super::{Effect, Store};

/// Which group the overlay is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    App,
    Project,
}

impl SettingsSection {
    #[cfg(feature = "devtools")]
    pub fn name(self) -> &'static str {
        match self {
            SettingsSection::App => "app",
            SettingsSection::Project => "project",
        }
    }
}

pub(crate) fn settings_activity(op: &SettingsOp) -> (&'static str, Option<String>) {
    match op {
        SettingsOp::Load => ("read_global", None),
        SettingsOp::Save(_) => ("update_global", None),
        SettingsOp::Forget { id } => ("forget_project", Some(id.clone())),
    }
}

/// One settings call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsOp {
    Load,
    Save(GlobalPatch),
    Forget { id: String },
}

/// What the bridge returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsDone {
    Loaded {
        global: GlobalSettings,
        project: Result<Option<ProjectSettings>, String>,
    },
    Saved(GlobalSettings),
    Forgot(String),
}

/// Overlay state. A fixed status line holds the last save sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsUi {
    pub open: bool,
    pub loading: bool,
    pub saving: bool,
    pub error: Option<String>,
    pub project_error: Option<String>,
    pub status: Option<String>,
    pub status_bad: bool,
    pub global: Option<GlobalSettings>,
    pub project: Option<ProjectSettings>,
    pub section: SettingsSection,
    pub cursor: usize,
    pub fetch_draft: String,
    pub fetch_focused: bool,
    pub confirm_remove: bool,
}

impl Default for SettingsUi {
    fn default() -> Self {
        Self {
            open: false,
            loading: false,
            saving: false,
            error: None,
            project_error: None,
            status: None,
            status_bad: false,
            global: None,
            project: None,
            section: SettingsSection::App,
            cursor: 0,
            fetch_draft: "15".to_string(),
            fetch_focused: false,
            confirm_remove: false,
        }
    }
}

const REMOVE_OFF: &str =
    "Removing a project is turned off unless the app data directory is a test directory.";

pub fn perform_settings(
    backend: &dyn Backend,
    project: Option<&str>,
    op: &SettingsOp,
) -> Result<SettingsDone, String> {
    match op {
        SettingsOp::Load => {
            let global = backend.read_global().map_err(|error| error.message)?;
            let project = match project {
                None => Ok(None),
                Some(id) => backend
                    .read_project(id)
                    .map(Some)
                    .map_err(|error| error.message),
            };
            Ok(SettingsDone::Loaded { global, project })
        }
        SettingsOp::Save(patch) => {
            let global = backend
                .update_global(patch)
                .map_err(|error| error.message)?;
            Ok(SettingsDone::Saved(global))
        }
        SettingsOp::Forget { id } => {
            backend.forget_project(id).map_err(|error| error.message)?;
            Ok(SettingsDone::Forgot(id.clone()))
        }
    }
}

impl Store {
    pub fn toggle_settings(&mut self) -> Effect {
        if self.settings.open {
            self.close_settings();
            Effect::None
        } else {
            self.open_settings()
        }
    }

    pub fn open_settings(&mut self) -> Effect {
        self.help = false;
        self.project_menu = false;
        self.menu = None;
        self.confirm = None;
        self.pages.branches_open = false;
        self.pages.history_open = false;
        self.pages.conflicts_open = false;
        self.pages.ask = None;
        self.remote.close_overlays();
        self.settings.open = true;
        self.settings.loading = true;
        self.settings.error = None;
        self.settings.confirm_remove = false;
        self.settings.fetch_focused = false;
        Effect::Settings(SettingsOp::Load)
    }

    pub fn close_settings(&mut self) {
        self.settings.open = false;
        self.settings.loading = false;
        self.settings.saving = false;
        self.settings.fetch_focused = false;
        self.settings.confirm_remove = false;
    }

    pub fn show_settings_section(&mut self, section: SettingsSection) {
        self.settings.section = section;
        self.settings.cursor = 0;
        self.settings.fetch_focused = false;
        self.settings.confirm_remove = false;
    }

    pub fn accept_settings(&mut self, done: SettingsDone) {
        match &done {
            SettingsDone::Loaded { project, .. } => {
                self.activity.succeed("read_global", "Settings are loaded.");
                match project {
                    Ok(_) => self
                        .activity
                        .succeed("read_project", "Project settings are loaded."),
                    Err(error) => self.activity.fail("read_project", error.clone()),
                }
            }
            SettingsDone::Saved(_) => {
                self.activity.succeed("update_global", "Saved settings.");
            }
            SettingsDone::Forgot(_) => {
                self.activity
                    .succeed("forget_project", "Removed the project from this app.");
            }
        }
        match done {
            SettingsDone::Loaded { global, project } => {
                self.settings.loading = false;
                self.settings.saving = false;
                self.settings.fetch_draft = global.fetch_interval_minutes.to_string();
                self.settings.global = Some(global);
                match project {
                    Ok(value) => {
                        self.settings.project = value;
                        self.settings.project_error = None;
                    }
                    Err(error) => {
                        self.settings.project = None;
                        self.settings.project_error = Some(error);
                    }
                }
                self.settings.error = None;
            }
            SettingsDone::Saved(global) => {
                self.settings.saving = false;
                self.settings.fetch_draft = global.fetch_interval_minutes.to_string();
                self.settings.global = Some(global);
                self.note_settings("Saved settings.", false);
            }
            SettingsDone::Forgot(id) => self.accept_forget(&id),
        }
    }

    pub fn fail_settings(&mut self, op: &SettingsOp, message: String) {
        let (id, _) = settings_activity(op);
        self.activity.fail(id, message.clone());
        if matches!(op, SettingsOp::Load) {
            self.activity.fail("read_project", message.clone());
        }
        self.settings.loading = false;
        self.settings.saving = false;
        self.settings.confirm_remove = false;
        match op {
            SettingsOp::Load if self.settings.global.is_none() => {
                self.settings.error = Some(message.clone());
            }
            SettingsOp::Load | SettingsOp::Save(_) | SettingsOp::Forget { .. } => {
                self.settings.status = Some(message.clone());
                self.settings.status_bad = true;
            }
        }
        self.toast_bad = true;
        self.toast = Some(message);
    }

    pub fn accept_forget(&mut self, id: &str) {
        self.projects.retain(|project| project.id != id);
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
            self.bump_data();
            self.workspace = None;
            self.worktree = None;
            self.clear_focus();
        }
        self.settings.project = None;
        self.settings.project_error = None;
        self.settings.confirm_remove = false;
        self.settings.saving = false;
        self.note_settings("Removed the project from this app.", false);
    }

    /// Keys while Settings is open. `None` means the overlay is closed.
    /// An open overlay consumes every key so it does not leak into a page.
    pub fn apply_settings_key(&mut self, key: &str) -> Option<Effect> {
        if !self.settings.open {
            return None;
        }
        if self.settings.confirm_remove {
            return Some(match key {
                "enter" | "y" => self.confirm_remove_project(),
                "escape" | "n" => {
                    self.settings.confirm_remove = false;
                    Effect::None
                }
                _ => Effect::None,
            });
        }
        if self.settings.fetch_focused {
            return Some(self.fetch_key(key));
        }
        Some(match key {
            "escape" | "," | "cmd-comma" => {
                self.close_settings();
                Effect::None
            }
            "up" => {
                self.move_settings_cursor(-1);
                Effect::None
            }
            "down" => {
                self.move_settings_cursor(1);
                Effect::None
            }
            "tab" | "shift-tab" => {
                let next = match self.settings.section {
                    SettingsSection::App => SettingsSection::Project,
                    SettingsSection::Project => SettingsSection::App,
                };
                self.show_settings_section(next);
                Effect::None
            }
            "enter" => self.activate_settings_cursor(),
            "left" => self.nudge_settings(-1),
            "right" => self.nudge_settings(1),
            _ => Effect::None,
        })
    }

    pub fn click_settings(&mut self, index: usize) -> Effect {
        self.settings.cursor = index;
        self.settings.fetch_focused = false;
        self.activate_settings_cursor()
    }

    fn move_settings_cursor(&mut self, step: i32) {
        let len = self.settings_len();
        if len == 0 {
            return;
        }
        let next = self.settings.cursor as i32 + step;
        self.settings.cursor = next.clamp(0, len as i32 - 1) as usize;
        self.settings.fetch_focused = false;
    }

    fn settings_len(&self) -> usize {
        match self.settings.section {
            SettingsSection::App => 5,
            SettingsSection::Project => 3,
        }
    }

    fn activate_settings_cursor(&mut self) -> Effect {
        match (self.settings.section, self.settings.cursor) {
            (SettingsSection::App, 0) => self.toggle_metrics(),
            (SettingsSection::App, 1) => self.toggle_error_reporting(),
            (SettingsSection::App, 2) => {
                self.settings.fetch_focused = true;
                if let Some(global) = &self.settings.global {
                    self.settings.fetch_draft = global.fetch_interval_minutes.to_string();
                }
                Effect::None
            }
            (SettingsSection::App, 3) => self.toggle_title_bar(),
            (SettingsSection::App, 4) => self.toggle_shadow(),
            (SettingsSection::Project, 0) | (SettingsSection::Project, 1) => {
                self.note_settings("This value is read-only.", true);
                Effect::None
            }
            (SettingsSection::Project, 2) => self.ask_remove_project(),
            _ => Effect::None,
        }
    }

    fn nudge_settings(&mut self, step: i32) -> Effect {
        match (self.settings.section, self.settings.cursor) {
            (SettingsSection::App, 2) => {
                let Some(global) = self.settings.global.clone() else {
                    return Effect::None;
                };
                let next = global
                    .fetch_interval_minutes
                    .saturating_add(i64::from(step));
                self.settings.fetch_draft = next.to_string();
                self.save_patch(GlobalPatch {
                    fetch_interval_minutes: Some(next),
                    ..GlobalPatch::default()
                })
            }
            (SettingsSection::App, 0 | 1 | 3 | 4) | (SettingsSection::Project, 2) => {
                self.activate_settings_cursor()
            }
            _ => Effect::None,
        }
    }

    fn toggle_metrics(&mut self) -> Effect {
        let Some(global) = self.settings.global.clone() else {
            return Effect::None;
        };
        self.save_patch(GlobalPatch {
            metrics_enabled: Some(!global.metrics_enabled),
            ..GlobalPatch::default()
        })
    }

    fn toggle_error_reporting(&mut self) -> Effect {
        let Some(global) = self.settings.global.clone() else {
            return Effect::None;
        };
        self.save_patch(GlobalPatch {
            error_reporting_enabled: Some(!global.error_reporting_enabled),
            ..GlobalPatch::default()
        })
    }

    fn toggle_title_bar(&mut self) -> Effect {
        let Some(global) = self.settings.global.clone() else {
            return Effect::None;
        };
        self.save_patch(GlobalPatch {
            native_title_bar: Some(!global.native_title_bar),
            ..GlobalPatch::default()
        })
    }

    fn toggle_shadow(&mut self) -> Effect {
        let Some(global) = self.settings.global.clone() else {
            return Effect::None;
        };
        self.save_patch(GlobalPatch {
            hide_window_shadow: Some(!global.hide_window_shadow),
            ..GlobalPatch::default()
        })
    }

    fn ask_remove_project(&mut self) -> Effect {
        if self.selected.is_none() {
            self.note_settings("Open a project before removing one.", true);
            return Effect::None;
        }
        let allowed = self
            .settings
            .project
            .as_ref()
            .is_some_and(|project| project.remove_allowed);
        if !allowed {
            self.note_settings(REMOVE_OFF, true);
            return Effect::None;
        }
        self.settings.confirm_remove = true;
        Effect::None
    }

    fn confirm_remove_project(&mut self) -> Effect {
        let Some(id) = self.selected.clone() else {
            self.settings.confirm_remove = false;
            return Effect::None;
        };
        let allowed = self
            .settings
            .project
            .as_ref()
            .is_some_and(|project| project.remove_allowed);
        if !allowed {
            self.settings.confirm_remove = false;
            self.note_settings(REMOVE_OFF, true);
            return Effect::None;
        }
        self.settings.saving = true;
        Effect::Settings(SettingsOp::Forget { id })
    }

    pub fn cancel_remove_project(&mut self) {
        self.settings.confirm_remove = false;
    }

    fn fetch_key(&mut self, key: &str) -> Effect {
        match key {
            "escape" => {
                self.settings.fetch_focused = false;
                if let Some(global) = &self.settings.global {
                    self.settings.fetch_draft = global.fetch_interval_minutes.to_string();
                }
                Effect::None
            }
            "enter" => self.save_fetch_draft(),
            "backspace" => {
                self.settings.fetch_draft.pop();
                Effect::None
            }
            "-" if !self.settings.fetch_draft.contains('-') => {
                self.settings.fetch_draft.insert(0, '-');
                Effect::None
            }
            other if other.len() == 1 && other.chars().all(|ch| ch.is_ascii_digit()) => {
                self.settings.fetch_draft.push_str(other);
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn save_fetch_draft(&mut self) -> Effect {
        let draft = self.settings.fetch_draft.trim();
        let Ok(minutes) = draft.parse::<i64>() else {
            self.note_settings(
                "Fetch interval needs a whole number of minutes. Use -1 to turn auto-fetch off.",
                true,
            );
            return Effect::None;
        };
        self.settings.fetch_focused = false;
        self.save_patch(GlobalPatch {
            fetch_interval_minutes: Some(minutes),
            ..GlobalPatch::default()
        })
    }

    fn save_patch(&mut self, patch: GlobalPatch) -> Effect {
        self.settings.saving = true;
        Effect::Settings(SettingsOp::Save(patch))
    }

    fn note_settings(&mut self, message: &str, bad: bool) {
        self.settings.status = Some(message.to_string());
        self.settings.status_bad = bad;
        self.toast_bad = bad;
        self.toast = Some(message.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::load;
    use gb_bridge::{FakeBackend, Settings};

    fn ready() -> (FakeBackend, Store) {
        let backend = FakeBackend::demo();
        let mut store = Store::default();
        store.select(Some("demo-project".to_string()));
        store.apply(load(&backend, store.selected.as_deref()));
        (backend, store)
    }

    #[test]
    fn fake_settings_round_trip_and_remove() {
        let (backend, mut store) = ready();
        let effect = store.open_settings();
        let SettingsOp::Load = effect_settings(effect) else {
            panic!("open loads settings");
        };
        let loaded =
            perform_settings(&backend, store.selected.as_deref(), &SettingsOp::Load).expect("load");
        store.accept_settings(loaded);
        let global = store.settings.global.clone().expect("global");
        assert!(!global.metrics_enabled);
        assert!(!global.error_reporting_enabled);
        assert_eq!(global.fetch_interval_minutes, 15);
        assert!(!global.feature_flags.is_empty());
        let flags = global.feature_flags.clone();
        let project = store.settings.project.clone().expect("project");
        assert_eq!(
            project.target_branch.as_deref(),
            Some("refs/remotes/origin/main")
        );
        assert!(!project.signing.enabled);
        assert!(project.remove_allowed);

        let effect = store.click_settings(0);
        let SettingsOp::Save(patch) = effect_settings(effect) else {
            panic!("metrics toggle saves");
        };
        assert_eq!(patch.metrics_enabled, Some(true));
        let saved = perform_settings(
            &backend,
            store.selected.as_deref(),
            &SettingsOp::Save(patch),
        )
        .expect("save");
        store.accept_settings(saved);
        assert_eq!(store.toast.as_deref(), Some("Saved settings."));
        assert!(
            store
                .settings
                .global
                .as_ref()
                .is_some_and(|item| item.metrics_enabled)
        );
        assert_eq!(
            store
                .settings
                .global
                .as_ref()
                .map(|item| &item.feature_flags),
            Some(&flags)
        );

        store.settings.cursor = 2;
        store.settings.fetch_focused = true;
        store.settings.fetch_draft = "30".to_string();
        let effect = store.apply_settings_key("enter").expect("fetch key");
        let SettingsOp::Save(patch) = effect_settings(effect) else {
            panic!("enter saves the interval");
        };
        let saved = perform_settings(
            &backend,
            store.selected.as_deref(),
            &SettingsOp::Save(patch),
        )
        .expect("save fetch");
        store.accept_settings(saved);
        assert_eq!(backend.read_global().unwrap().fetch_interval_minutes, 30);
        assert!(backend.read_global().unwrap().metrics_enabled);

        store.show_settings_section(SettingsSection::Project);
        store.settings.cursor = 2;
        assert!(store.confirm_remove_via_key());
        let effect = store.apply_settings_key("enter").expect("confirm");
        let SettingsOp::Forget { id } = effect_settings(effect) else {
            panic!("confirm removes");
        };
        assert_eq!(id, "demo-project");
        let done = perform_settings(&backend, Some(&id), &SettingsOp::Forget { id: id.clone() })
            .expect("forget");
        store.accept_settings(done);
        assert!(backend.list_projects().unwrap().projects.is_empty());
        assert!(store.selected.is_none());
        assert_eq!(
            store.toast.as_deref(),
            Some("Removed the project from this app.")
        );
    }

    #[test]
    fn an_open_settings_overlay_consumes_other_keys() {
        let (_backend, mut store) = ready();
        store.settings.open = true;
        let effect = store.apply_key("b");
        assert_eq!(effect, Effect::None);
        assert!(!store.pages.branches_open);
        assert!(store.settings.open);
    }

    impl Store {
        fn confirm_remove_via_key(&mut self) -> bool {
            self.settings.section = SettingsSection::Project;
            self.settings.cursor = 2;
            let _ = self.apply_settings_key("enter");
            self.settings.confirm_remove
        }
    }

    fn effect_settings(effect: Effect) -> SettingsOp {
        match effect {
            Effect::Settings(op) => op,
            other => panic!("expected settings, got {other:?}"),
        }
    }
}
