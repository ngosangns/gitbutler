//! Manual snapshots, the file summary, restore, and undo.
//!
//! Temp app data and a local bare remote. No network.
//! The diff is the committed workspace tree, so the mutation is a commit.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use gb_bridge::{Anchor, Backend, FileStatus, InProcessBackend, PINNED_VERSION, SelectionSpec};
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
    let status = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success(), "bare init");
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-u", "origin", "main"]);
    work
}

fn enter_workspace(backend: &InProcessBackend, id: &str, root: &Path) {
    let mut ctx = open_ctx(id, root);
    but_api::legacy::virtual_branches::set_base_branch(
        &mut ctx,
        "origin/main".to_string(),
        Some("origin".to_string()),
    )
    .unwrap_or_else(|error| panic!("enter workspace: {error:#}"));
    if let Err(error) = backend.create_lane(id, "lane-a") {
        let mut ctx = open_ctx(id, root);
        but_api::legacy::virtual_branches::create_virtual_branch(
            &mut ctx,
            gitbutler_branch::BranchCreateRequest {
                name: Some("lane-a".to_string()),
                order: None,
            },
        )
        .unwrap_or_else(|fallback| panic!("create lane ({error:?}) fallback: {fallback:#}"));
    }
}

fn lane_titles(backend: &InProcessBackend, id: &str) -> Vec<String> {
    backend
        .open_project(id)
        .unwrap()
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == "lane-a")
        .map(|branch| {
            branch
                .commits
                .iter()
                .map(|commit| commit.title.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn snapshot_commit_restore_and_undo() {
    let isolated = isolated();
    let root = isolated.dir.path();
    let work = setup_repo(root);
    let backend = matching_backend();
    let id = backend.add_project(work.to_str().unwrap()).unwrap().id;
    backend
        .set_target(&id, "refs/remotes/origin/main", Some("origin"))
        .unwrap_or_else(|error| panic!("set target: {error:?}"));
    enter_workspace(&backend, &id, root);

    std::fs::write(work.join("keeper.txt"), "keep\n").unwrap();
    let seeded = backend
        .commit_create(
            &id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("keeper.txt")],
            "seed keeper",
        )
        .unwrap_or_else(|error| panic!("seed: {error:?}"));
    assert!(seeded.rejected.is_empty(), "{:?}", seeded.rejected);

    let checkpoint = backend
        .create_snapshot(&id, "checkpoint")
        .unwrap_or_else(|error| panic!("snapshot: {error:?}"));
    assert_eq!(checkpoint.operation, "OnDemandSnapshot");
    assert_eq!(checkpoint.title, "checkpoint");
    assert!(checkpoint.restorable);
    assert!(checkpoint.created_at_ms > 0);

    std::fs::write(work.join("keeper.txt"), "changed\n").unwrap();
    let edited = backend
        .commit_create(
            &id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("keeper.txt")],
            "edit keeper",
        )
        .unwrap_or_else(|error| panic!("edit: {error:?}"));
    assert!(edited.rejected.is_empty(), "{:?}", edited.rejected);
    assert_eq!(
        std::fs::read_to_string(work.join("keeper.txt")).unwrap(),
        "changed\n"
    );

    let listed = backend
        .list_snapshots(&id, 50)
        .unwrap_or_else(|error| panic!("list: {error:?}"));
    assert!(
        listed.iter().any(|snapshot| {
            snapshot.sha == checkpoint.sha && snapshot.title == "checkpoint" && snapshot.restorable
        }),
        "{listed:?}"
    );
    let head = listed
        .iter()
        .find(|snapshot| snapshot.operation == "CreateCommit")
        .unwrap_or_else(|| panic!("no CreateCommit: {listed:?}"));
    let diff = backend
        .snapshot_diff(&id, &head.sha)
        .unwrap_or_else(|error| panic!("diff: {error:?}"));
    assert!(
        diff.iter().any(|change| {
            change.path == "keeper.txt"
                && matches!(change.status, FileStatus::Modified | FileStatus::Added)
        }),
        "{diff:?}"
    );

    backend
        .restore_snapshot(&id, &checkpoint.sha)
        .unwrap_or_else(|error| panic!("restore: {error:?}"));
    assert_eq!(
        std::fs::read_to_string(work.join("keeper.txt")).unwrap(),
        "keep\n"
    );
    let titles = lane_titles(&backend, &id);
    assert!(
        !titles.iter().any(|title| title == "edit keeper"),
        "{titles:?}"
    );
    let listed = backend.list_snapshots(&id, 50).unwrap();
    assert!(
        listed
            .iter()
            .any(|snapshot| snapshot.operation == "RestoreFromSnapshot"),
        "{listed:?}"
    );

    backend
        .undo_last(&id)
        .unwrap_or_else(|error| panic!("undo: {error:?}"));
    assert_eq!(
        std::fs::read_to_string(work.join("keeper.txt")).unwrap(),
        "changed\n"
    );
    let titles = lane_titles(&backend, &id);
    assert!(
        titles.iter().any(|title| title == "edit keeper"),
        "{titles:?}"
    );
    let listed = backend.list_snapshots(&id, 50).unwrap();
    assert!(
        listed.iter().any(|snapshot| {
            snapshot.operation == "RestoreFromSnapshotViaUndo" && !snapshot.restorable
        }),
        "{listed:?}"
    );
    guard(root);
}
