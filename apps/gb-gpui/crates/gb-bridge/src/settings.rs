//! App and project settings.
//!
//! Global values go through `but_settings` in the app config directory.
//! `AppSettings::load` fills missing telemetry keys from upstream defaults,
//! where usage numbers and error reports are on. This module reports a missing
//! key as off and does not write the file on a read. A save writes the chosen
//! value explicitly, including when it matches the upstream default.
//!
//! There is no scale or theme field on `AppSettings`. The appearance values
//! that do exist are the native title bar and the window shadow.
//!
//! Signing is read from git config only. The report says whether signing is on
//! and whether a key name is set. It does not contain the key, and it does not
//! run a signer.

use std::path::{Path, PathBuf};

use but_settings::AppSettingsWithDiskSync;
use but_settings::api::{FetchUpdate, TelemetryUpdate, UiUpdate};
use but_settings::app_settings::FeatureFlags;

use crate::read::{self, VersionSource};
use crate::{BridgeError, Compat, Result, WorkspaceSummary, compat_error};

/// One read-only feature flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureFlag {
    pub name: String,
    pub label: String,
    pub value: String,
}

/// Settings that apply to every project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalSettings {
    pub metrics_enabled: bool,
    pub error_reporting_enabled: bool,
    pub fetch_interval_minutes: i64,
    pub native_title_bar: bool,
    pub hide_window_shadow: bool,
    pub feature_flags: Vec<FeatureFlag>,
}

/// A partial save. Absent fields stay as they are. Feature flags are not here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalPatch {
    pub metrics_enabled: Option<bool>,
    pub error_reporting_enabled: Option<bool>,
    pub fetch_interval_minutes: Option<i64>,
    pub native_title_bar: Option<bool>,
    pub hide_window_shadow: Option<bool>,
}

/// What git config says about commit signing. The key text is never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningReport {
    pub enabled: bool,
    pub key_present: bool,
    pub summary: String,
}

/// Target branch, signing, and whether removal is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSettings {
    pub target_branch: Option<String>,
    pub signing: SigningReport,
    pub remove_allowed: bool,
}

impl GlobalSettings {
    /// In-memory defaults. Telemetry is off. Fetch stays at 15 minutes.
    pub fn defaults() -> Self {
        Self {
            metrics_enabled: false,
            error_reporting_enabled: false,
            fetch_interval_minutes: 15,
            native_title_bar: false,
            hide_window_shadow: false,
            feature_flags: default_flags(),
        }
    }

    pub fn apply(&mut self, patch: &GlobalPatch) {
        if let Some(value) = patch.metrics_enabled {
            self.metrics_enabled = value;
        }
        if let Some(value) = patch.error_reporting_enabled {
            self.error_reporting_enabled = value;
        }
        if let Some(value) = patch.fetch_interval_minutes {
            self.fetch_interval_minutes = value;
        }
        if let Some(value) = patch.native_title_bar {
            self.native_title_bar = value;
        }
        if let Some(value) = patch.hide_window_shadow {
            self.hide_window_shadow = value;
        }
    }
}

impl SigningReport {
    pub fn off() -> Self {
        Self::from_parts(false, false)
    }

    pub fn from_parts(enabled: bool, key_present: bool) -> Self {
        let summary = match (enabled, key_present) {
            (false, false) => "Commit signing is off.",
            (true, true) => "Commit signing is on, and a signing key is configured.",
            (true, false) => "Commit signing is on. No signing key is named in git config.",
            (false, true) => "A signing key is named, and commit signing is off.",
        };
        Self {
            enabled,
            key_present,
            summary: summary.to_string(),
        }
    }
}

fn default_flags() -> Vec<FeatureFlag> {
    vec![
        flag("watch_mode", "Watch mode", "auto"),
        flag("write_commit_evolution", "Write commit evolution", "Off"),
        flag("tui_file_browser", "Terminal file browser", "Off"),
        flag("worktree_manipulation", "Worktree manipulation", "Off"),
    ]
}

fn flag(name: &str, label: &str, value: &str) -> FeatureFlag {
    FeatureFlag {
        name: name.to_string(),
        label: label.to_string(),
        value: value.to_string(),
    }
}

fn on_off(value: bool) -> String {
    if value {
        "On".to_string()
    } else {
        "Off".to_string()
    }
}

fn flags_from(flags: &FeatureFlags) -> Vec<FeatureFlag> {
    vec![
        flag("watch_mode", "Watch mode", &flags.watch_mode),
        flag(
            "write_commit_evolution",
            "Write commit evolution",
            &on_off(flags.write_commit_evolution),
        ),
        flag(
            "tui_file_browser",
            "Terminal file browser",
            &on_off(flags.tui_file_browser),
        ),
        flag(
            "worktree_manipulation",
            "Worktree manipulation",
            &on_off(flags.worktree_manipulation),
        ),
    ]
}

/// Read and update settings. Default methods refuse, so a backend that forgot
/// them fails closed.
pub trait Settings: Send + Sync {
    fn read_global(&self) -> Result<GlobalSettings> {
        Err(BridgeError::not_ready(
            "This backend does not read settings",
        ))
    }

    fn update_global(&self, patch: &GlobalPatch) -> Result<GlobalSettings> {
        let _ = patch;
        Err(BridgeError::not_ready(
            "This backend does not save settings",
        ))
    }

    fn read_project(&self, id: &str) -> Result<ProjectSettings> {
        let _ = id;
        Err(BridgeError::not_ready(
            "This backend does not read project settings",
        ))
    }

    fn forget_project(&self, id: &str) -> Result<()> {
        let _ = id;
        Err(BridgeError::not_ready(
            "Removing a project is turned off unless the app data directory is a test directory.",
        ))
    }
}

/// `true` only when both directories sit inside `data_root`.
/// `None` is never safe. Component-wise, so a prefix of the directory name
/// does not count.
pub fn removal_allowed(data_root: Option<&Path>, data: &Path, config: &Path) -> bool {
    let Some(root) = data_root else {
        return false;
    };
    if root.as_os_str().is_empty() {
        return false;
    }
    data.starts_with(root) && config.starts_with(root)
}

pub(crate) fn removal_is_safe() -> bool {
    let Ok(root) = std::env::var("E2E_TEST_APP_DATA_DIR") else {
        return false;
    };
    if root.is_empty() {
        return false;
    }
    let Ok(data) = but_path::app_data_dir() else {
        return false;
    };
    let Ok(config) = but_path::app_config_dir() else {
        return false;
    };
    removal_allowed(Some(Path::new(&root)), &data, &config)
}

/// Panic when a test override is set and a resolved directory is outside it.
/// A process without the override returns without reading the real directories.
pub(crate) fn guard_test_dir() {
    let Ok(root) = std::env::var("E2E_TEST_APP_DATA_DIR") else {
        return;
    };
    if root.is_empty() {
        return;
    }
    let root = PathBuf::from(&root);
    let data = but_path::app_data_dir().unwrap_or_default();
    let config = but_path::app_config_dir().unwrap_or_default();
    if !data.starts_with(&root) || !config.starts_with(&root) {
        panic!(
            "settings path escaped the test directory: data {} config {} root {}",
            data.display(),
            config.display(),
            root.display()
        );
    }
}

pub(crate) mod disk {
    use super::*;

    pub(crate) fn read_global(source: VersionSource) -> Result<GlobalSettings> {
        guard_test_dir();
        let env = read::environment(source)?;
        if env.compat != Compat::Match {
            return Err(compat_error(&env));
        }
        read::settings_safe_to_load(&env)?;
        let path = env.config_dir.join("settings.json");
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            BridgeError::not_ready(format!("Could not read settings.json ({error})"))
        })?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        let metrics = json_bool(&value, "appMetricsEnabled").unwrap_or(false);
        let errors = json_bool(&value, "appErrorReportingEnabled").unwrap_or(false);
        let stored = AppSettingsWithDiskSync::new_with_customization(&env.config_dir, None)
            .map_err(|error| {
                BridgeError::internal(format!("Could not read GitButler settings: {error}"))
            })?;
        let settings = stored.get().map_err(|error| {
            BridgeError::internal(format!("Could not read GitButler settings: {error}"))
        })?;
        let fetch = i64::try_from(settings.fetch.auto_fetch_interval_minutes).unwrap_or(15);
        Ok(GlobalSettings {
            metrics_enabled: metrics,
            error_reporting_enabled: errors,
            fetch_interval_minutes: fetch,
            native_title_bar: settings.ui.use_native_title_bar,
            hide_window_shadow: settings.ui.no_shadow,
            feature_flags: flags_from(&settings.feature_flags),
        })
    }

    pub(crate) fn update_global(
        source: VersionSource,
        patch: &GlobalPatch,
    ) -> Result<GlobalSettings> {
        guard_test_dir();
        let env = read::environment(source)?;
        if env.compat != Compat::Match {
            return Err(compat_error(&env));
        }
        read::settings_safe_to_load(&env)?;
        let stored = AppSettingsWithDiskSync::new_with_customization(&env.config_dir, None)
            .map_err(|error| {
                BridgeError::internal(format!("Could not read GitButler settings: {error}"))
            })?;
        if patch.metrics_enabled.is_some() || patch.error_reporting_enabled.is_some() {
            stored
                .update_telemetry(TelemetryUpdate {
                    app_metrics_enabled: patch.metrics_enabled,
                    app_error_reporting_enabled: patch.error_reporting_enabled,
                })
                .map_err(|error| {
                    BridgeError::internal(format!("Could not save telemetry settings: {error}"))
                })?;
            // A value that matches the upstream default is omitted from the
            // file. Write the chosen bools so the next read does not treat a
            // missing key as off.
            persist_telemetry(
                &env.config_dir.join("settings.json"),
                patch.metrics_enabled,
                patch.error_reporting_enabled,
            )?;
        }
        if let Some(minutes) = patch.fetch_interval_minutes {
            let minutes = isize::try_from(minutes)
                .map_err(|_| BridgeError::not_ready("Fetch interval is out of range."))?;
            stored
                .update_fetch(FetchUpdate {
                    auto_fetch_interval_minutes: Some(minutes),
                })
                .map_err(|error| {
                    BridgeError::internal(format!("Could not save the fetch interval: {error}"))
                })?;
        }
        if patch.native_title_bar.is_some() || patch.hide_window_shadow.is_some() {
            stored
                .update_ui(UiUpdate {
                    use_native_title_bar: patch.native_title_bar,
                    no_shadow: patch.hide_window_shadow,
                })
                .map_err(|error| {
                    BridgeError::internal(format!("Could not save appearance settings: {error}"))
                })?;
        }
        guard_test_dir();
        read_global(source)
    }

    pub(crate) fn read_project(source: VersionSource, id: &str) -> Result<ProjectSettings> {
        guard_test_dir();
        let worktree = read::project_worktree(source, id)?;
        let signing = signing_report(&worktree)?;
        let target = match read::open_project(source, id) {
            Ok(workspace) => target_of(workspace),
            Err(_) => None,
        };
        Ok(ProjectSettings {
            target_branch: target,
            signing,
            remove_allowed: removal_is_safe(),
        })
    }

    pub(crate) fn forget_guard() -> Result<()> {
        guard_test_dir();
        if removal_is_safe() {
            Ok(())
        } else {
            Err(BridgeError::not_ready(
                "Removing a project is turned off unless the app data directory is a test directory.",
            ))
        }
    }
}

fn target_of(workspace: WorkspaceSummary) -> Option<String> {
    workspace.target_branch
}

fn json_bool(value: &serde_json::Value, name: &str) -> Option<bool> {
    value
        .get("telemetry")
        .and_then(|telemetry| telemetry.get(name))
        .and_then(|item| item.as_bool())
}

fn persist_telemetry(path: &Path, metrics: Option<bool>, errors: Option<bool>) -> Result<()> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        BridgeError::internal(format!("Could not read settings.json ({error})"))
    })?;
    let mut value: serde_json::Value = serde_json::from_str(&raw).map_err(|error| {
        BridgeError::internal(format!("Could not parse settings.json ({error})"))
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        BridgeError::internal("settings.json is not an object, so it was left unchanged.")
    })?;
    let telemetry = object
        .entry("telemetry")
        .or_insert_with(|| serde_json::json!({}));
    let telemetry = telemetry.as_object_mut().ok_or_else(|| {
        BridgeError::internal("settings.json telemetry is not an object, so it was left unchanged.")
    })?;
    if let Some(metrics) = metrics {
        telemetry.insert(
            "appMetricsEnabled".to_string(),
            serde_json::Value::Bool(metrics),
        );
    }
    if let Some(errors) = errors {
        telemetry.insert(
            "appErrorReportingEnabled".to_string(),
            serde_json::Value::Bool(errors),
        );
    }
    let text = serde_json::to_string_pretty(&value).map_err(|error| {
        BridgeError::internal(format!("Could not write settings.json ({error})"))
    })?;
    std::fs::write(path, format!("{text}\n")).map_err(|error| {
        BridgeError::internal(format!("Could not write settings.json ({error})"))
    })?;
    Ok(())
}

fn signing_report(worktree: &Path) -> Result<SigningReport> {
    let repo = gix::open(worktree).map_err(|error| {
        BridgeError::internal(format!(
            "Could not open {} to read signing settings: {error}",
            worktree.display()
        ))
    })?;
    let config = repo.config_snapshot();
    let enabled = config_flag(&config, "gitbutler.signCommits")
        .or_else(|| config_flag(&config, "commit.gpgsign"))
        .unwrap_or(false);
    let key_present = config.string("user.signingKey").is_some();
    Ok(SigningReport::from_parts(enabled, key_present))
}

fn config_flag(config: &gix::config::Snapshot<'_>, key: &str) -> Option<bool> {
    config.boolean(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_allowed_requires_both_directories_inside_the_root() {
        let root = Path::new("/tmp/gb-gpui-settings-root");
        let data = root.join("data");
        let config = root.join("config");
        assert!(!removal_allowed(None, &data, &config));
        assert!(!removal_allowed(Some(Path::new("")), &data, &config));
        assert!(!removal_allowed(
            Some(Path::new("/elsewhere")),
            &data,
            &config
        ));
        assert!(!removal_allowed(Some(Path::new("/tmp/gb")), &data, &config));
        assert!(!removal_allowed(Some(&data), &data, &config));
        assert!(removal_allowed(Some(root), &data, &config));
    }

    #[test]
    fn signing_summary_never_includes_a_key() {
        let report = SigningReport::from_parts(true, true);
        assert!(report.enabled);
        assert!(report.key_present);
        assert!(!report.summary.contains("BEGIN"));
        assert_eq!(SigningReport::off().summary, "Commit signing is off.");
    }
}
