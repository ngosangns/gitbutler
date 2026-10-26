//! Branch list, apply, unapply, and delete.
//!
//! Temp app data, a fixture repo, and a local bare remote. No network.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use gb_bridge::{Backend, BranchPlace, BridgeErrorKind, InProcessBackend, PINNED_VERSION};
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
    if !data.starts_with(root) {
        panic!(
            "but_path::app_data_dir() is {}, outside test dir {}",
            data.display(),
            root.display()
        );
    }
    if !config.starts_with(root) {
        panic!(
            "but_path::app_config_dir() is {}, outside test dir {}",
            config.display(),
            root.display()
        );
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Gb Gpui")
        .env("GIT_AUTHOR_EMAIL", "gb-gpui@example.com")
        .env("GIT_COMMITTER_NAME", "Gb Gpui")
        .env("GIT_COMMITTER_EMAIL", "gb-gpui@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git {args:?} in {} failed\n{stderr}{stdout}",
        dir.display()
    );
    stdout.to_string()
}

fn write_settings() {
    let config = but_path::app_config_dir().expect("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("settings.json"),
        "{\"telemetry\":{\"migratedFromLegacy\":true}}\n",
    )
    .unwrap();
}

fn matching_backend() -> InProcessBackend {
    InProcessBackend::with_installed_version_for_test(Some(PINNED_VERSION)).unwrap()
}

fn open_ctx(id: &str, root: &Path) -> but_ctx::Context {
    let project = gitbutler_project::dangerously_list_projects_without_migration()
        .unwrap()
        .into_iter()
        .find(|project| project.id.to_string() == id)
        .unwrap_or_else(|| panic!("project {id} is not in the list"))
        .migrated()
        .unwrap();
    but_ctx::Context::new(
        project.git_dir(),
        but_path::app_config_dir().unwrap(),
        Some(root.join("cache")),
    )
    .expect("context")
}

/// `side` is one commit ahead of and one behind `origin/main`. `topic` exists only on the remote.
fn setup_repo(root: &Path) -> PathBuf {
    guard(root);
    write_settings();
    let work = root.join("work");
    let bare = root.join("remote.git");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.name", "Gb Gpui"]);
    git(&work, &["config", "user.email", "gb-gpui@example.com"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "README.md"]);
    git(&work, &["commit", "-m", "init"]);

    git(&work, &["checkout", "-b", "side"]);
    std::fs::write(work.join("side.txt"), "side\n").unwrap();
    git(&work, &["add", "side.txt"]);
    git(&work, &["commit", "-m", "side commit"]);
    git(&work, &["checkout", "main"]);
    std::fs::write(work.join("main2.txt"), "main2\n").unwrap();
    git(&work, &["add", "main2.txt"]);
    git(&work, &["commit", "-m", "second on main"]);

    let status = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success(), "bare init");
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-u", "origin", "main"]);
    git(&work, &["push", "origin", "side"]);

    git(&work, &["checkout", "-b", "topic"]);
    std::fs::write(work.join("topic.txt"), "topic\n").unwrap();
    git(&work, &["add", "topic.txt"]);
    git(&work, &["commit", "-m", "topic"]);
    git(&work, &["push", "-u", "origin", "topic"]);
    git(&work, &["checkout", "main"]);
    git(&work, &["branch", "-D", "topic"]);
    work
}

fn enter_workspace(id: &str, root: &Path) {
    let mut ctx = open_ctx(id, root);
    but_api::legacy::virtual_branches::set_base_branch(
        &mut ctx,
        "origin/main".to_string(),
        Some("origin".to_string()),
    )
    .unwrap_or_else(|error| panic!("enter workspace: {error:#}"));
}

fn wait_event(rx: &mpsc::Receiver<&str>, wanted: &str) -> Vec<String> {
    let mut names = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(name) => {
                let hit = name == wanted;
                names.push(name.to_string());
                if hit {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    names
}

#[test]
fn apply_lists_the_branch_then_unapply_and_delete() {
    let isolated = isolated();
    let root = isolated.dir.path();
    let work = setup_repo(root);
    let backend = matching_backend();
    let id = backend.add_project(work.to_str().unwrap()).unwrap().id;
    backend
        .set_target(&id, "refs/remotes/origin/main", Some("origin"))
        .unwrap_or_else(|error| panic!("set target: {error:?}"));
    enter_workspace(&id, root);

    let listed = backend
        .list_branches(&id)
        .unwrap_or_else(|error| panic!("list: {error:?}"));
    let side = listed
        .iter()
        .find(|row| row.name == "side")
        .unwrap_or_else(|| panic!("side missing: {listed:?}"));
    assert!(!side.applied, "{side:?}");
    assert_eq!(side.place, BranchPlace::Both, "{side:?}");
    assert_eq!(side.ref_name, "refs/heads/side");
    assert_eq!(side.ahead, Some(1), "{side:?}");
    assert_eq!(side.behind, Some(1), "{side:?}");
    assert_eq!(side.last_commit, "side commit", "{side:?}");
    assert!(side.author.contains("Gb Gpui"), "{side:?}");
    let topic = listed
        .iter()
        .find(|row| row.name == "topic")
        .unwrap_or_else(|| panic!("topic missing: {listed:?}"));
    assert!(!topic.applied, "{topic:?}");
    assert_eq!(topic.place, BranchPlace::Remote, "{topic:?}");
    assert!(topic.ref_name.contains("topic"), "{topic:?}");

    let details = backend
        .branch_details(&id, "refs/heads/side")
        .unwrap_or_else(|error| panic!("details: {error:?}"));
    assert!(
        details
            .commits
            .iter()
            .any(|commit| commit.title == "side commit"),
        "{details:?}"
    );

    let (tx, rx) = mpsc::channel();
    let _watch = backend
        .watch(
            &id,
            Box::new(move |event| {
                let _ = tx.send(event.name());
            }),
        )
        .unwrap();
    let applied = backend
        .apply_branch(&id, "refs/heads/side")
        .unwrap_or_else(|error| panic!("apply: {error:?}"));
    assert!(!applied.already_applied, "{applied:?}");
    assert!(
        applied.names.iter().any(|name| name == "side"),
        "{applied:?}"
    );
    let names = wait_event(&rx, "workspaceActivity");
    assert!(
        names.iter().any(|name| name == "workspaceActivity"),
        "{names:?}"
    );
    let listed = backend.list_branches(&id).unwrap();
    let side = listed.iter().find(|row| row.name == "side").unwrap();
    assert!(side.applied, "{side:?}");

    let refused = backend.delete_local_branch(&id, "side").unwrap_err();
    assert!(refused.message.contains("Unapply"), "{}", refused.message);
    assert_eq!(
        git(&work, &["rev-parse", "--verify", "refs/heads/side"])
            .trim()
            .len(),
        40
    );

    backend
        .unapply_branch(&id, "side")
        .unwrap_or_else(|error| panic!("unapply: {error:?}"));
    let listed = backend.list_branches(&id).unwrap();
    let side = listed
        .iter()
        .find(|row| row.name == "side")
        .unwrap_or_else(|| panic!("side vanished after unapply: {listed:?}"));
    assert!(!side.applied, "{side:?}");
    assert!(
        git(&work, &["rev-parse", "--verify", "refs/heads/side"])
            .trim()
            .len()
            == 40
    );

    backend
        .delete_local_branch(&id, "refs/heads/side")
        .unwrap_or_else(|error| panic!("delete: {error:?}"));
    let gone = Command::new("git")
        .args(["rev-parse", "--verify", "refs/heads/side"])
        .current_dir(&work)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap();
    assert!(!gone.success(), "local side ref still exists");
    assert!(
        backend
            .list_branches(&id)
            .unwrap()
            .iter()
            .all(|row| row.name != "side" || row.place == BranchPlace::Remote)
    );
    guard(root);
}

#[test]
fn list_branches_refuses_a_version_mismatch() {
    let _isolated = isolated();
    let mismatch = InProcessBackend::with_installed_version_for_test(Some("9.9.9")).unwrap();
    let error = mismatch.list_branches("unused").unwrap_err();
    assert_eq!(error.kind, BridgeErrorKind::Compat);
    assert!(
        error.message.contains("9.9.9") && error.message.contains(PINNED_VERSION),
        "{}",
        error.message
    );
}
