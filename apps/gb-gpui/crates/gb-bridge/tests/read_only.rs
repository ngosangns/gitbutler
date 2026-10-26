//! Adapter tests. Every test points `E2E_TEST_APP_DATA_DIR` at a temp directory
//! before the first `but_path` call and holds a process-wide lock so the
//! override cannot leak onto the real GitButler data directory.

use std::collections::{BTreeMap, HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use but_core::RepositoryExt as _;
use gb_bridge::{Backend, BridgeEvent, Compat, FakeBackend, InProcessBackend, PINNED_VERSION};
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

/// Panics when GitButler's redirected directories are outside `root`.
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

struct Fixture {
    work: PathBuf,
    id: String,
    gitbutler: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Gb Gpui")
        .env("GIT_AUTHOR_EMAIL", "gb-gpui@example.com")
        .env("GIT_COMMITTER_NAME", "Gb Gpui")
        .env("GIT_COMMITTER_EMAIL", "gb-gpui@example.com")
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
    // AppSettings::load creates this file when it is missing, and rewrites it
    // when a legacy settings.json is migrated. Writing the flag first keeps
    // Context::new from touching the file.
    std::fs::write(
        config.join("settings.json"),
        "{\"telemetry\":{\"migratedFromLegacy\":true}}\n",
    )
    .unwrap();
}

fn setup_fixture(root: &Path) -> Fixture {
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
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(
        &work,
        &["update-ref", "refs/remotes/origin/main", head.trim()],
    );

    let outcome = gitbutler_project::add(&work).expect("add project");
    let project = outcome.try_project().expect("added").migrated().unwrap();
    let id = project.id.to_string();
    let git_dir = project.git_dir().to_path_buf();
    let gitbutler = gix::open(&work).unwrap().gitbutler_storage_path().unwrap();

    let mut ctx = but_ctx::Context::new(
        &git_dir,
        but_path::app_config_dir().unwrap(),
        Some(root.join("cache")),
    )
    .expect("context");
    // Same order as GitButler: register the project, then set the target through
    // `set_base_branch`, which creates the workspace commit. A bare target write
    // leaves `main` owning the base commit, so a new stack cannot be created.
    but_api::legacy::virtual_branches::set_base_branch(
        &mut ctx,
        "origin/main".to_string(),
        Some("origin".to_string()),
    )
    .expect("set target");
    for (name, file) in [("lane-a", "a.txt"), ("lane-b", "b.txt")] {
        but_api::legacy::virtual_branches::create_virtual_branch(
            &mut ctx,
            gitbutler_branch::BranchCreateRequest {
                name: Some(name.to_string()),
                order: None,
            },
        )
        .unwrap_or_else(|error| panic!("create {name}: {error:#}"));
        std::fs::write(work.join(file), format!("{name}\n")).unwrap();
        let reference = gix::refs::FullName::try_from(format!("refs/heads/{name}")).unwrap();
        // An empty lane is a reference sitting on the workspace base. Inserting
        // below that reference is what moves the branch onto the new commit.
        // Inserting above leaves the ref on the base, so the lane stays empty.
        let created = but_api::commit::create::commit_create_only(
            &mut ctx,
            but_rebase::graph_rebase::mutate::RelativeTo::Reference(reference),
            but_rebase::graph_rebase::mutate::InsertSide::Below,
            vec![but_core::DiffSpec {
                path: file.into(),
                previous_path: None,
                hunk_headers: Vec::new(),
            }],
            but_api::commit::json::ChangesSource::Head,
            format!("commit {name}"),
            but_core::DryRun::No,
        )
        .unwrap_or_else(|error| panic!("commit {name}: {error:#}"));
        assert!(
            created.rejected_specs.is_empty(),
            "commit {name} rejected: {}",
            created
                .rejected_specs
                .iter()
                .map(|(reason, spec)| format!("{reason:?} {}", spec.path))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(
            created.new_commit.is_some(),
            "commit {name} produced no commit"
        );
    }
    drop(ctx);
    guard(root);
    Fixture {
        work,
        id,
        gitbutler,
    }
}

fn matching_backend() -> InProcessBackend {
    InProcessBackend::with_installed_version_for_test(Some(PINNED_VERSION)).unwrap()
}

/// Content fingerprint. `-shm` is recorded too; the comparison may ignore it.
fn fingerprint(root: &Path) -> BTreeMap<String, (u64, u64)> {
    let mut map = BTreeMap::new();
    if !root.exists() {
        return map;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                let mut hasher = DefaultHasher::new();
                bytes.hash(&mut hasher);
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                map.insert(rel, (bytes.len() as u64, hasher.finish()));
            }
        }
    }
    map
}

fn snapshot(env_root: &Path, gitbutler: &Path) -> HashMap<String, BTreeMap<String, (u64, u64)>> {
    let mut all = HashMap::new();
    let data = but_path::app_data_dir().unwrap();
    let config = but_path::app_config_dir().unwrap();
    guard(env_root);
    all.insert("data".to_string(), fingerprint(&data));
    all.insert("config".to_string(), fingerprint(&config));
    all.insert("gitbutler".to_string(), fingerprint(gitbutler));
    all
}

/// A WAL file is a 32-byte header plus one frame per changed page. A header
/// with no frames holds no database pages.
fn wal_has_no_frames(digest: Option<&(u64, u64)>) -> bool {
    digest.is_none_or(|(len, _)| *len <= 32)
}

/// SQLite creates `but.sqlite-wal` and `but.sqlite-shm` when a read-only
/// connection opens a WAL database whose sidecar files were checkpointed away.
/// The `-shm` file is the wal-index. A header-only `-wal` has no page frames.
/// Either difference is not a content write when `but.sqlite` itself is unchanged
/// and the WAL has no frames before and after. A longer WAL is a content write.
fn assert_unchanged(
    before: &HashMap<String, BTreeMap<String, (u64, u64)>>,
    after: &HashMap<String, BTreeMap<String, (u64, u64)>>,
) {
    for (label, left) in before {
        let right = after.get(label).expect(label);
        let mut diffs = Vec::new();
        for (path, digest) in left {
            match right.get(path) {
                Some(other) if other == digest => {}
                Some(other) => diffs.push(format!("changed {path}: {digest:?} -> {other:?}")),
                None => diffs.push(format!("removed {path}")),
            }
        }
        for path in right.keys() {
            if !left.contains_key(path) {
                diffs.push(format!("added {path}"));
            }
        }
        if diffs.is_empty() {
            continue;
        }
        let sqlite_same = left.get("but.sqlite") == right.get("but.sqlite");
        let only_sidecars = diffs
            .iter()
            .all(|diff| diff.contains("but.sqlite-shm") || diff.contains("but.sqlite-wal"));
        let wal_empty = wal_has_no_frames(left.get("but.sqlite-wal"))
            && wal_has_no_frames(right.get("but.sqlite-wal"));
        assert!(
            sqlite_same && only_sidecars && wal_empty,
            "{label} content changed: {diffs:?}\nsqlite_same={sqlite_same} wal_before={:?} wal_after={:?}",
            left.get("but.sqlite-wal"),
            right.get("but.sqlite-wal")
        );
        eprintln!(
            "gb-bridge note: ignored header-only WAL sidecar churn in {label}: {diffs:?} wal_after={:?}",
            right.get("but.sqlite-wal")
        );
    }
}

#[test]
fn lists_and_opens_two_stacks() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let backend = matching_backend();
    let list = backend.list_projects().unwrap();
    assert_eq!(list.projects.len(), 1, "{list:?}");
    assert_eq!(list.projects[0].id, fixture.id);
    assert!(
        list.projects[0].path.contains("work"),
        "{}",
        list.projects[0].path
    );

    let workspace = backend.open_project(&fixture.id).unwrap();
    let mut names: Vec<String> = workspace
        .stacks
        .iter()
        .flat_map(|stack| stack.names().map(str::to_string))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["lane-a".to_string(), "lane-b".to_string()],
        "{workspace:?}"
    );
    assert!(
        workspace
            .target_branch
            .as_deref()
            .unwrap_or("")
            .contains("origin/main"),
        "{workspace:?}"
    );
    for stack in &workspace.stacks {
        assert_eq!(stack.commit_count(), 1, "{stack:?}");
        assert_eq!(stack.tip_sha().len(), 7, "{stack:?}");
    }
}

#[test]
fn read_path_does_not_write() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let before = snapshot(isolated.dir.path(), &fixture.gitbutler);
    let backend = matching_backend();
    backend.list_projects().unwrap();
    backend.open_project(&fixture.id).unwrap();
    let watched = backend.watch(&fixture.id, Box::new(|_| {})).expect("watch");
    drop(watched);
    std::thread::sleep(Duration::from_millis(400));
    let after = snapshot(isolated.dir.path(), &fixture.gitbutler);
    assert_unchanged(&before, &after);
}

#[test]
fn watch_reports_worktree_and_git_changes() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let backend = matching_backend();
    let (tx, rx) = mpsc::channel();
    let _watch = backend
        .watch(
            &fixture.id,
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(fixture.work.join("watched.txt"), "watched\n").unwrap();
    std::fs::write(fixture.work.join("README.md"), "hello\nchanged\n").unwrap();
    let view = wait_for_paths(&rx, &["watched.txt", "README.md"], Duration::from_secs(8));
    assert!(
        view.files
            .iter()
            .any(|file| file.path.ends_with("watched.txt")),
        "new file missing: {view:?}"
    );
    assert!(
        view.files
            .iter()
            .any(|file| file.path.ends_with("README.md")),
        "modified file missing: {view:?}"
    );
    // The workspace branch refuses a plain `git commit` (pre-commit hook).
    // Point HEAD at a new branch on the same commit, then commit there.
    // The branch switch and the commit are both git activity the watcher reports.
    git(&fixture.work, &["checkout", "-B", "gb-record"]);
    git(&fixture.work, &["add", "watched.txt", "README.md"]);
    git(&fixture.work, &["commit", "-m", "record the watch"]);
    assert!(
        wait_for_git(&rx, Duration::from_secs(8)),
        "no git or workspace activity"
    );
}

fn wait_for_paths(
    rx: &mpsc::Receiver<BridgeEvent>,
    want: &[&str],
    timeout: Duration,
) -> gb_bridge::WorktreeView {
    let deadline = Instant::now() + timeout;
    let mut latest = None;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(Duration::from_millis(250))) {
            Ok(BridgeEvent::Worktree(view)) => {
                let ready = want.iter().all(|path| {
                    view.files.iter().any(|file| file.path.ends_with(path))
                        || view.changed_paths.iter().any(|file| file.ends_with(path))
                });
                if ready {
                    return view;
                }
                latest = Some(view);
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    panic!("no worktree event with {want:?}; last={latest:?}");
}

fn wait_for_git(rx: &mpsc::Receiver<BridgeEvent>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(Duration::from_millis(250))) {
            Ok(event) if event.refreshes_workspace() => return true,
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
    false
}

#[test]
fn mismatch_and_missing_database_do_not_write() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let before = snapshot(isolated.dir.path(), &fixture.gitbutler);

    let mismatch = InProcessBackend::with_installed_version_for_test(Some("9.9.9")).unwrap();
    let error = mismatch.list_projects().unwrap_err();
    assert!(
        error.message.contains("9.9.9") && error.message.contains(PINNED_VERSION),
        "{}",
        error.message
    );
    let open_error = mismatch.open_project(&fixture.id).unwrap_err();
    assert!(
        open_error.message.contains("9.9.9"),
        "{}",
        open_error.message
    );
    assert_unchanged(&before, &snapshot(isolated.dir.path(), &fixture.gitbutler));

    let db = fixture.gitbutler.join("but.sqlite");
    assert!(db.is_file(), "fixture should have created {}", db.display());
    std::fs::remove_file(&db).unwrap();
    for extra in ["but.sqlite-wal", "but.sqlite-shm"] {
        let path = fixture.gitbutler.join(extra);
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let error = matching_backend().open_project(&fixture.id).unwrap_err();
    assert!(
        error
            .message
            .contains("Open this project in GitButler first"),
        "{}",
        error.message
    );
    assert!(!db.exists(), "missing database was recreated");
}

#[test]
fn missing_project_list_is_empty_and_not_created() {
    let isolated = isolated();
    guard(isolated.dir.path());
    write_settings();
    let backend = matching_backend();
    let list = backend.list_projects().unwrap();
    assert!(list.projects.is_empty());
    assert!(list.reason.is_some(), "{list:?}");
    let projects = but_path::app_data_dir().unwrap().join("projects.json");
    assert!(!projects.exists(), "projects.json was created");
}

#[test]
fn fake_backend_lists_and_emits() {
    let backend = FakeBackend::demo();
    let env = backend.environment().unwrap();
    assert_eq!(env.compat, Compat::Match);
    assert_eq!(env.pinned, PINNED_VERSION);
    let list = backend.list_projects().unwrap();
    assert_eq!(list.projects.len(), 1);
    let workspace = backend.open_project(&list.projects[0].id).unwrap();
    assert_eq!(workspace.stacks.len(), 3);
    assert!(
        workspace
            .stacks
            .iter()
            .any(|stack| stack.commit_count() >= 2)
    );
    let files = backend.worktree(&list.projects[0].id).unwrap();
    assert!(files.files.len() >= 6, "{}", files.files.len());
    assert!(
        files
            .files
            .iter()
            .any(|file| file.assignment_label() == "split")
    );

    let (tx, rx) = mpsc::channel();
    let _watch = backend
        .watch(
            &list.projects[0].id,
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    backend.trigger_fake(&list.projects[0].id, "git").unwrap();
    let event = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(event.refreshes_workspace(), "{event:?}");

    let mismatch = FakeBackend::mismatched("9.9.9");
    let error = mismatch.list_projects().unwrap_err();
    assert!(error.message.contains("9.9.9"));
}

fn changed_gitbutler_files(
    before: &BTreeMap<String, (u64, u64)>,
    after: &BTreeMap<String, (u64, u64)>,
) -> Vec<String> {
    let mut names = Vec::new();
    for (path, digest) in after {
        if before.get(path) != Some(digest) {
            names.push(format!("changed {path}"));
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            names.push(format!("removed {path}"));
        }
    }
    names.sort();
    names
}

#[test]
fn watcher_writes_only_the_sqlite_family() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let before = snapshot(isolated.dir.path(), &fixture.gitbutler);
    let backend = matching_backend();
    let (tx, rx) = mpsc::channel();
    let watch = backend
        .watch(
            &fixture.id,
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
    std::fs::write(fixture.work.join("assigned.txt"), "assigned\n").unwrap();
    let _ = wait_for_paths(&rx, &["assigned.txt"], Duration::from_secs(8));
    drop(watch);
    std::thread::sleep(Duration::from_millis(300));
    let after = snapshot(isolated.dir.path(), &fixture.gitbutler);
    assert_eq!(
        before.get("data"),
        after.get("data"),
        "app data changed: {:?}",
        changed_gitbutler_files(before.get("data").unwrap(), after.get("data").unwrap())
    );
    assert_eq!(
        before.get("config"),
        after.get("config"),
        "settings changed"
    );
    let diffs = changed_gitbutler_files(
        before.get("gitbutler").unwrap(),
        after.get("gitbutler").unwrap(),
    );
    assert!(
        diffs.iter().all(|diff| diff.contains("but.sqlite")),
        "watcher wrote outside the sqlite family: {diffs:?}"
    );
    assert!(
        diffs.iter().any(|diff| diff.contains("but.sqlite")),
        "expected the project database to record hunk assignments, diffs={diffs:?}"
    );
}

#[test]
fn preflight_blocks_the_watcher_without_touching_files() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let before = snapshot(isolated.dir.path(), &fixture.gitbutler);

    let mismatch = InProcessBackend::with_installed_version_for_test(Some("9.9.9")).unwrap();
    let error = mismatch.watch(&fixture.id, Box::new(|_| {})).unwrap_err();
    assert!(
        error.message.contains("9.9.9") && error.message.contains(PINNED_VERSION),
        "{}",
        error.message
    );
    assert_unchanged(&before, &snapshot(isolated.dir.path(), &fixture.gitbutler));

    {
        let db = fixture.gitbutler.join("but.sqlite");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute(
                "DELETE FROM __diesel_schema_migrations WHERE version = (SELECT MAX(version) FROM __diesel_schema_migrations)",
                [],
            )
            .unwrap();
    }
    let pending = snapshot(isolated.dir.path(), &fixture.gitbutler);
    let error = matching_backend()
        .watch(&fixture.id, Box::new(|_| {}))
        .unwrap_err();
    assert!(
        error
            .message
            .contains("Open this project in GitButler first"),
        "{}",
        error.message
    );
    assert_unchanged(&pending, &snapshot(isolated.dir.path(), &fixture.gitbutler));

    let settings = but_path::app_config_dir().unwrap().join("settings.json");
    std::fs::remove_file(&settings).unwrap();
    let error = matching_backend()
        .watch(&fixture.id, Box::new(|_| {}))
        .unwrap_err();
    assert!(
        error.message.contains("will not create"),
        "{}",
        error.message
    );
    assert!(!settings.exists(), "settings.json was recreated");
}

#[test]
fn diffs_cover_modified_new_and_binary_files() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    std::fs::write(fixture.work.join("a.txt"), "lane-a\nchanged\n").unwrap();
    std::fs::write(fixture.work.join("new.txt"), "brand new\n").unwrap();
    std::fs::write(fixture.work.join("logo.bin"), [0, 1, 2, 0, 255]).unwrap();
    let backend = matching_backend();
    let modified = backend
        .file_diff(
            &fixture.id,
            gb_bridge::DiffRequest::Worktree {
                path: "a.txt".to_string(),
            },
        )
        .unwrap();
    assert!(!modified.binary, "{modified:?}");
    assert!(modified.patch.contains("changed"), "{}", modified.patch);
    let added = backend
        .file_diff(
            &fixture.id,
            gb_bridge::DiffRequest::Worktree {
                path: "new.txt".to_string(),
            },
        )
        .unwrap();
    assert!(added.patch.contains('+'), "{}", added.patch);
    let binary = backend
        .file_diff(
            &fixture.id,
            gb_bridge::DiffRequest::Worktree {
                path: "logo.bin".to_string(),
            },
        )
        .unwrap();
    assert!(binary.binary, "{binary:?}");
    assert!(binary.patch.is_empty(), "{binary:?}");
}

#[test]
fn commit_details_include_message_author_and_files() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let backend = matching_backend();
    let workspace = backend.open_project(&fixture.id).unwrap();
    let commit = workspace
        .stacks
        .iter()
        .flat_map(|stack| stack.branches.iter())
        .find(|branch| branch.name == "lane-a")
        .and_then(|branch| branch.commits.first())
        .expect("lane-a commit");
    let details = backend.commit_details(&fixture.id, &commit.sha).unwrap();
    assert!(
        details.message.contains("commit lane-a"),
        "{}",
        details.message
    );
    assert!(details.author.contains("Gb Gpui"), "{}", details.author);
    assert!(!details.parents.is_empty(), "{details:?}");
    assert!(
        details.files.iter().any(|file| file.path == "a.txt"),
        "{details:?}"
    );
}

#[test]
fn lanes_list_several_commits_on_two_stacks() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    std::fs::write(fixture.work.join("a.txt"), "lane-a\nagain\n").unwrap();
    let git_dir = gix::open(&fixture.work).unwrap().git_dir().to_owned();
    let mut ctx = but_ctx::Context::new(
        &git_dir,
        but_path::app_config_dir().unwrap(),
        Some(isolated.dir.path().join("cache")),
    )
    .unwrap();
    let reference = gix::refs::FullName::try_from("refs/heads/lane-a").unwrap();
    let created = but_api::commit::create::commit_create_only(
        &mut ctx,
        but_rebase::graph_rebase::mutate::RelativeTo::Reference(reference),
        but_rebase::graph_rebase::mutate::InsertSide::Below,
        vec![but_core::DiffSpec {
            path: "a.txt".into(),
            previous_path: None,
            hunk_headers: Vec::new(),
        }],
        but_api::commit::json::ChangesSource::Head,
        "commit lane-a again".to_string(),
        but_core::DryRun::No,
    )
    .unwrap();
    assert!(
        created.new_commit.is_some(),
        "commit_create_only did not return a commit"
    );
    drop(ctx);

    let workspace = matching_backend().open_project(&fixture.id).unwrap();
    assert!(workspace.stacks.len() >= 2, "{workspace:?}");
    let lane_a = workspace
        .stacks
        .iter()
        .find(|stack| stack.names().any(|name| name == "lane-a"))
        .expect("lane-a");
    assert!(lane_a.commit_count() >= 2, "{lane_a:?}");
    let commits = &lane_a.branches[0].commits;
    assert!(commits.iter().any(|commit| commit.title.contains("again")));
    assert!(commits.iter().all(|commit| commit.short_sha.len() == 7));
    assert!(commits.iter().all(|commit| commit.author.contains("Gb")));
}
