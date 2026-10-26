//! Edit mode and conflict resolution.
//!
//! Temp app data and a local bare remote. No network.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use gb_bridge::{
    Anchor, Backend, ConflictChoice, FileChoice, InProcessBackend, PINNED_VERSION, SelectionSpec,
    UpstreamStrategy,
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

fn show_file(dir: &Path, sha: &str, path: &str) -> String {
    git(dir, &["show", &format!("{sha}:{path}")])
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

fn setup_repo(root: &Path) -> (PathBuf, PathBuf) {
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
    (work, bare)
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

fn lane_tip(backend: &InProcessBackend, id: &str) -> String {
    backend
        .open_project(id)
        .unwrap()
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == "lane-a")
        .and_then(|branch| branch.commits.first())
        .map(|commit| commit.sha.clone())
        .unwrap_or_else(|| panic!("lane-a has no commit"))
}

#[test]
fn save_edit_updates_the_commit_and_abort_leaves_it() {
    let isolated = isolated();
    let root = isolated.dir.path();
    let (work, _bare) = setup_repo(root);
    let backend = matching_backend();
    let id = backend.add_project(work.to_str().unwrap()).unwrap().id;
    backend
        .set_target(&id, "refs/remotes/origin/main", Some("origin"))
        .unwrap_or_else(|error| panic!("set target: {error:?}"));
    enter_workspace(&backend, &id, root);
    std::fs::write(work.join("lane.txt"), "lane\n").unwrap();
    let created = backend
        .commit_create(
            &id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("lane.txt")],
            "lane commit",
        )
        .unwrap_or_else(|error| panic!("commit: {error:?}"));
    assert!(created.rejected.is_empty(), "{:?}", created.rejected);
    let sha = created.new_sha.expect("sha");
    assert_eq!(show_file(&work, &sha, "lane.txt"), "lane\n");

    let status = backend
        .enter_edit_mode(&id, &sha)
        .unwrap_or_else(|error| panic!("enter: {error:?}"));
    assert!(status.editing, "{status:?}");
    assert_eq!(
        status.commit_sha.as_deref(),
        Some(sha.as_str()),
        "{status:?}"
    );
    let blocked = backend
        .commit_create(
            &id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("lane.txt")],
            "while editing",
        )
        .unwrap_err();
    assert!(blocked.message.contains("edit mode"), "{}", blocked.message);

    std::fs::write(work.join("lane.txt"), "edited\n").unwrap();
    backend
        .save_edit_and_return_to_workspace(&id)
        .unwrap_or_else(|error| panic!("save: {error:?}"));
    let saved = lane_tip(&backend, &id);
    assert_ne!(saved, sha);
    assert_eq!(show_file(&work, &saved, "lane.txt"), "edited\n");
    let status = backend.edit_status(&id).unwrap();
    assert!(!status.editing, "{status:?}");

    let again = backend
        .enter_edit_mode(&id, &saved)
        .unwrap_or_else(|error| panic!("enter again: {error:?}"));
    assert!(again.editing);
    std::fs::write(work.join("lane.txt"), "dropped\n").unwrap();
    let refused = backend
        .abort_edit_and_return_to_workspace(&id, false)
        .unwrap_err();
    assert!(refused.message.contains("drop"), "{}", refused.message);
    assert!(backend.edit_status(&id).unwrap().editing);
    assert_eq!(show_file(&work, &saved, "lane.txt"), "edited\n");

    backend
        .abort_edit_and_return_to_workspace(&id, true)
        .unwrap_or_else(|error| panic!("abort: {error:?}"));
    assert!(!backend.edit_status(&id).unwrap().editing);
    assert_eq!(lane_tip(&backend, &id), saved);
    assert_eq!(show_file(&work, &saved, "lane.txt"), "edited\n");
    assert_eq!(
        std::fs::read_to_string(work.join("lane.txt")).unwrap(),
        "edited\n"
    );
    backend
        .create_lane(&id, "lane-b")
        .unwrap_or_else(|error| panic!("lane after abort: {error:?}"));
    guard(root);
}

#[test]
fn resolve_a_rebased_commit_ours_and_theirs() {
    let isolated = isolated();
    let root = isolated.dir.path();
    let (work, bare) = setup_repo(root);
    std::fs::write(work.join("a.txt"), "base\n").unwrap();
    std::fs::write(work.join("b.txt"), "base\n").unwrap();
    git(&work, &["add", "a.txt", "b.txt"]);
    git(&work, &["commit", "-m", "base files"]);
    git(&work, &["push", "origin", "main"]);

    let backend = matching_backend();
    let id = backend.add_project(work.to_str().unwrap()).unwrap().id;
    backend
        .set_target(&id, "refs/remotes/origin/main", Some("origin"))
        .unwrap_or_else(|error| panic!("set target: {error:?}"));
    enter_workspace(&backend, &id, root);
    std::fs::write(work.join("a.txt"), "lane\n").unwrap();
    std::fs::write(work.join("b.txt"), "lane\n").unwrap();
    let created = backend
        .commit_create(
            &id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("a.txt"), SelectionSpec::whole("b.txt")],
            "lane edit",
        )
        .unwrap_or_else(|error| panic!("lane commit: {error:?}"));
    assert!(created.rejected.is_empty(), "{:?}", created.rejected);
    let sha = created.new_sha.expect("sha");
    assert_eq!(show_file(&work, &sha, "a.txt"), "lane\n");
    assert_eq!(show_file(&work, &sha, "b.txt"), "lane\n");

    let other = root.join("other");
    git(
        root,
        &["clone", bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    git(&other, &["config", "user.name", "Gb Gpui"]);
    git(&other, &["config", "user.email", "gb-gpui@example.com"]);
    std::fs::write(other.join("a.txt"), "upstream\n").unwrap();
    std::fs::write(other.join("b.txt"), "upstream\n").unwrap();
    git(&other, &["add", "a.txt", "b.txt"]);
    git(&other, &["commit", "-m", "upstream"]);
    git(&other, &["push", "origin", "main"]);

    backend
        .fetch(&id)
        .unwrap_or_else(|error| panic!("fetch: {error:?}"));
    let integrated = backend
        .integrate(&id, UpstreamStrategy::Rebase)
        .unwrap_or_else(|error| panic!("integrate: {error:?}"));
    let conflicted = backend
        .conflicted_commits(&id)
        .unwrap_or_else(|error| panic!("conflicted: {error:?}"));
    assert!(
        conflicted.iter().any(|commit| commit.title == "lane edit"),
        "integrate={integrated:?}\nconflicted={conflicted:?}"
    );
    let sha = conflicted
        .iter()
        .find(|commit| commit.title == "lane edit")
        .unwrap()
        .sha
        .clone();

    let report = backend
        .resolve_commit_conflict_hunks(
            &id,
            &sha,
            &[
                FileChoice {
                    path: "a.txt".to_string(),
                    choice: ConflictChoice::Ours,
                },
                FileChoice {
                    path: "b.txt".to_string(),
                    choice: ConflictChoice::Theirs,
                },
            ],
        )
        .unwrap_or_else(|error| panic!("resolve: {error:?}"));
    assert_ne!(report.new_sha, sha);
    assert!(!report.still_conflicted, "{report:?}");
    assert_eq!(show_file(&work, &report.new_sha, "a.txt"), "upstream\n");
    assert_eq!(show_file(&work, &report.new_sha, "b.txt"), "lane\n");
    let after = backend.conflicted_commits(&id).unwrap();
    assert!(
        after.iter().all(|commit| commit.sha != report.new_sha),
        "{after:?}"
    );
    guard(root);
}
