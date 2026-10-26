//! Settings round trip in a temp app-data directory.
//!
//! `E2E_TEST_APP_DATA_DIR` is set before the first `but_path` call. The test
//! refuses to continue if the resolved directories leave that temp root.
//! No network, no keychain, and no signer.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use gb_bridge::{
    Backend, GlobalPatch, InProcessBackend, PINNED_VERSION, Settings, removal_allowed,
};
use tempfile::TempDir;

static LOCK: Mutex<()> = Mutex::new(());

struct Isolated {
    _lock: MutexGuard<'static, ()>,
    dir: TempDir,
}

fn isolated() -> Isolated {
    let lock = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let dir = TempDir::new().expect("temp dir");
    unsafe {
        std::env::set_var("E2E_TEST_APP_DATA_DIR", dir.path());
    }
    guard(dir.path());
    Isolated { _lock: lock, dir }
}

fn guard(root: &Path) {
    let data = but_path::app_data_dir().expect("app data dir");
    let config = but_path::app_config_dir().expect("config dir");
    assert!(
        data.starts_with(root),
        "app data {} is outside {}",
        data.display(),
        root.display()
    );
    assert!(
        config.starts_with(root),
        "config {} is outside {}",
        config.display(),
        root.display()
    );
}

fn write_settings(root: &Path) {
    let config = but_path::app_config_dir().expect("config");
    assert!(config.starts_with(root));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("settings.json"),
        "{\"telemetry\":{\"migratedFromLegacy\":true}}\n",
    )
    .unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Gb Gpui")
        .env("GIT_AUTHOR_EMAIL", "gb-gpui@example.com")
        .env("GIT_COMMITTER_NAME", "Gb Gpui")
        .env("GIT_COMMITTER_EMAIL", "gb-gpui@example.com")
        .output()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed\n{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn settings_round_trip_stays_in_the_temp_dir() {
    let isolated = isolated();
    let root = isolated.dir.path();
    write_settings(root);
    guard(root);

    let backend =
        InProcessBackend::with_installed_version_for_test(Some(PINNED_VERSION)).expect("backend");
    let before = backend.read_global().expect("read");
    assert!(!before.metrics_enabled);
    assert!(!before.error_reporting_enabled);
    assert_eq!(before.fetch_interval_minutes, 15);
    assert!(
        before
            .feature_flags
            .iter()
            .any(|flag| flag.name == "watch_mode" && flag.value == "auto")
    );
    let flags = before.feature_flags.clone();

    let patch = GlobalPatch {
        metrics_enabled: Some(true),
        error_reporting_enabled: Some(false),
        fetch_interval_minutes: Some(-1),
        native_title_bar: Some(true),
        hide_window_shadow: None,
    };
    let saved = backend.update_global(&patch).expect("save");
    assert!(saved.metrics_enabled);
    assert!(!saved.error_reporting_enabled);
    assert_eq!(saved.fetch_interval_minutes, -1);
    assert!(saved.native_title_bar);
    assert_eq!(saved.feature_flags, flags);

    let again =
        InProcessBackend::with_installed_version_for_test(Some(PINNED_VERSION)).expect("reload");
    let loaded = again.read_global().expect("reload read");
    assert!(loaded.metrics_enabled);
    assert!(!loaded.error_reporting_enabled);
    assert_eq!(loaded.fetch_interval_minutes, -1);
    assert!(loaded.native_title_bar);
    assert_eq!(loaded.feature_flags, flags);

    let config = but_path::app_config_dir().expect("config");
    let settings_path = config.join("settings.json");
    let raw = std::fs::read_to_string(&settings_path).expect("settings file");
    assert!(settings_path.starts_with(root));
    assert!(raw.contains("appMetricsEnabled"));
    assert!(raw.contains("appErrorReportingEnabled"));
    guard(root);

    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.name", "Gb Gpui"]);
    git(&work, &["config", "user.email", "gb-gpui@example.com"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "README.md"]);
    git(&work, &["commit", "-m", "init"]);
    let added = again.add_project(work.to_str().unwrap()).expect("add");
    git(&work, &["config", "commit.gpgsign", "true"]);
    git(&work, &["config", "user.signingkey", "gb-test-sign"]);
    let project = again.read_project(&added.id).expect("project settings");
    assert!(project.signing.enabled);
    assert!(project.signing.key_present);
    assert!(!project.signing.summary.contains("gb-test-sign"));
    assert!(!project.signing.summary.contains("BEGIN"));
    assert!(project.remove_allowed);

    again.forget_project(&added.id).expect("forget");
    let listed = again.list_projects().expect("list");
    assert!(listed.projects.iter().all(|project| project.id != added.id));
    guard(root);

    let outside = PathBuf::from("/tmp/gb-gpui-not-the-root");
    assert!(!removal_allowed(None, root, root));
    assert!(!removal_allowed(Some(&outside), root, root));
    assert!(removal_allowed(
        Some(root),
        &root.join("data"),
        &root.join("config")
    ));
}
