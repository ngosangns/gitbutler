//! Project setup, fetch, integrate, push, and the askpass broker.
//!
//! Temp app data, a fixture repo, and a local bare remote. No network.
//! A file remote cannot complete GitButler's askpass socket handshake, so the
//! broker test calls `AskpassBroker::submit_prompt` directly.
//!
//! `set_target` records the remote-tracking branch only. Entering the managed
//! workspace, which fetch/integrate/push of a lane need, uses the same
//! `set_base_branch` call the desktop uses when it is not in single-branch mode.
use std::collections::{BTreeMap, HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use but_core::RepositoryExt as _;
use gb_bridge::{
    Anchor, AskpassKind, Backend, ForcePush, InProcessBackend, PINNED_VERSION, SelectionSpec,
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

fn rev(dir: &Path, spec: &str) -> String {
    git(dir, &["rev-parse", spec]).trim().to_string()
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

/// A repo with `origin` pointing at a local bare remote that has `main` and `topic`.
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
    git(&work, &["checkout", "-b", "topic"]);
    std::fs::write(work.join("topic.txt"), "topic\n").unwrap();
    git(&work, &["add", "topic.txt"]);
    git(&work, &["commit", "-m", "topic"]);
    git(&work, &["push", "-u", "origin", "topic"]);
    git(&work, &["checkout", "main"]);
    git(&work, &["branch", "-D", "topic"]);
    (work, bare)
}

fn add_work(backend: &InProcessBackend, work: &Path) -> String {
    backend
        .add_project(work.to_str().unwrap())
        .unwrap_or_else(|error| panic!("add project: {error:?}"))
        .id
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

fn enter_workspace(backend: &InProcessBackend, id: &str, work: &Path, root: &Path) {
    {
        let mut ctx = open_ctx(id, root);
        but_api::legacy::virtual_branches::set_base_branch(
            &mut ctx,
            "origin/main".to_string(),
            Some("origin".to_string()),
        )
        .unwrap_or_else(|error| panic!("enter workspace: {error:#}"));
    }
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
    std::fs::write(work.join("lane.txt"), "lane\n").unwrap();
    let created = backend
        .commit_create(
            id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("lane.txt")],
            "lane commit",
        )
        .unwrap_or_else(|error| panic!("commit lane: {error:?}"));
    assert!(created.rejected.is_empty(), "{:?}", created.rejected);
    assert!(created.new_sha.is_some(), "{:?}", created.workspace);
}

fn fingerprint(root: &Path) -> BTreeMap<String, (u64, u64)> {
    let mut map = BTreeMap::new();
    if !root.exists() {
        return map;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).unwrap();
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
                map.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    (bytes.len() as u64, hasher.finish()),
                );
            }
        }
    }
    map
}

fn snapshot(root: &Path, gitbutler: &Path) -> HashMap<String, BTreeMap<String, (u64, u64)>> {
    guard(root);
    let mut all = HashMap::new();
    all.insert(
        "data".to_string(),
        fingerprint(&but_path::app_data_dir().unwrap()),
    );
    all.insert(
        "config".to_string(),
        fingerprint(&but_path::app_config_dir().unwrap()),
    );
    all.insert("gitbutler".to_string(), fingerprint(gitbutler));
    all
}

fn wal_has_no_frames(digest: Option<&(u64, u64)>) -> bool {
    digest.is_none_or(|(len, _)| *len <= 32)
}

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
            "{label} content changed: {diffs:?}"
        );
    }
}

fn wait_event(rx: &mpsc::Receiver<&str>, wanted: &[&str]) -> Vec<String> {
    let mut names = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(name) => {
                let hit = wanted.contains(&name);
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
fn setup_lists_remote_branches_and_add_is_idempotent() {
    let isolated = isolated();
    let root = isolated.dir.path();
    let (work, bare) = setup_repo(root);
    let backend = matching_backend();
    let id = add_work(&backend, &work);
    assert_eq!(backend.add_project(work.to_str().unwrap()).unwrap().id, id);

    let branches = backend.remote_branches(&id).unwrap();
    let names: Vec<_> = branches
        .iter()
        .map(|branch| branch.full_name.as_str())
        .collect();
    assert!(names.contains(&"refs/remotes/origin/main"), "{names:?}");
    assert!(names.contains(&"refs/remotes/origin/topic"), "{names:?}");

    let base = backend
        .set_target(&id, "refs/remotes/origin/main", Some("origin"))
        .unwrap_or_else(|error| panic!("set target: {error:?}"));
    assert_eq!(base.short_name, "main");
    assert_eq!(base.remote_name, "origin");
    assert_eq!(base.push_remote_name, "origin");
    assert!(
        base.branch_name.contains("origin/main"),
        "{}",
        base.branch_name
    );
    assert_eq!(base.behind, 0);
    assert!(
        base.remote_url.contains("remote.git"),
        "{}",
        base.remote_url
    );
    assert!(
        base.push_remote_url.contains("remote.git") || base.push_remote_name == "origin",
        "{base:?}"
    );

    backend.delete_project(&id).unwrap();
    assert!(
        backend
            .list_projects()
            .unwrap()
            .projects
            .iter()
            .all(|project| project.id != id)
    );
    assert!(work.join("README.md").is_file());
    assert!(bare.join("HEAD").is_file());
    guard(root);
}

#[test]
fn fetch_integrate_and_push_against_a_bare_remote() {
    let isolated = isolated();
    let root = isolated.dir.path();
    let (work, bare) = setup_repo(root);
    let backend = matching_backend();
    let id = add_work(&backend, &work);
    backend
        .set_target(&id, "refs/remotes/origin/main", Some("origin"))
        .unwrap_or_else(|error| panic!("set target: {error:?}"));
    enter_workspace(&backend, &id, &work, root);

    let other = root.join("other");
    git(
        root,
        &["clone", bare.to_str().unwrap(), other.to_str().unwrap()],
    );
    git(&other, &["config", "user.name", "Gb Gpui"]);
    git(&other, &["config", "user.email", "gb-gpui@example.com"]);
    std::fs::write(other.join("upstream.txt"), "upstream\n").unwrap();
    git(&other, &["add", "upstream.txt"]);
    git(&other, &["commit", "-m", "upstream"]);
    git(&other, &["push", "origin", "main"]);

    let (tx, rx) = mpsc::channel();
    let _watch = backend
        .watch(
            &id,
            Box::new(move |event| {
                let _ = tx.send(event.name());
            }),
        )
        .unwrap();

    let report = backend
        .fetch(&id)
        .unwrap_or_else(|error| panic!("fetch: {error:?}"));
    assert_eq!(report.status.upstream_count, 1, "{report:?}");
    assert_eq!(report.status.summary, "1 new upstream commit");
    assert!(report.status.last_fetched_ms.is_some(), "{report:?}");
    assert!(
        report
            .status
            .commits
            .iter()
            .any(|commit| commit.title == "upstream"),
        "{:?}",
        report.status.commits
    );
    let names = wait_event(&rx, &["gitFetch", "workspaceActivity"]);
    assert!(
        names
            .iter()
            .any(|name| name == "gitFetch" || name == "workspaceActivity"),
        "{names:?}"
    );

    let before = rev(&work, "refs/heads/lane-a");
    let preview = backend
        .integrate_preview(&id, UpstreamStrategy::Rebase)
        .unwrap_or_else(|error| panic!("preview: {error:?}"));
    assert!(
        preview.resolutions.iter().any(|lane| lane.lane == "lane-a"),
        "{preview:?}"
    );
    assert_eq!(
        rev(&work, "refs/heads/lane-a"),
        before,
        "preview moved the lane"
    );

    backend
        .integrate(&id, UpstreamStrategy::Rebase)
        .unwrap_or_else(|error| panic!("integrate: {error:?}"));
    let tip = rev(&work, "refs/heads/lane-a");
    let parent = rev(&work, &format!("{tip}^"));
    let upstream = rev(&work, "refs/remotes/origin/main");
    assert_eq!(
        parent, upstream,
        "rebased lane should sit on the fetched upstream"
    );
    git(&work, &["merge-base", "--is-ancestor", &upstream, &tip]);

    let pushed = backend
        .push(&id, "lane-a", ForcePush::No)
        .unwrap_or_else(|error| panic!("push: {error:?}"));
    assert!(
        pushed.summary.contains("lane-a")
            || pushed
                .branches
                .iter()
                .any(|branch| branch.name.contains("lane")),
        "{pushed:?}"
    );
    assert_eq!(rev(&bare, "refs/heads/lane-a"), tip);

    git(&other, &["fetch", "origin"]);
    git(&other, &["checkout", "-b", "lane-a", "origin/lane-a"]);
    std::fs::write(other.join("other.txt"), "other\n").unwrap();
    git(&other, &["add", "other.txt"]);
    git(&other, &["commit", "-m", "diverge"]);
    git(&other, &["push", "origin", "lane-a"]);
    let diverged = rev(&bare, "refs/heads/lane-a");
    assert_ne!(diverged, tip);
    // The remote is strictly ahead of the pushed tip, so pushing those same
    // commits is a no-op. A new local commit whose parent is still that tip
    // makes the histories diverge, and a regular push is rejected.
    std::fs::write(work.join("local.txt"), "local\n").unwrap();
    backend
        .commit_create(
            &id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("local.txt")],
            "local ahead",
        )
        .unwrap_or_else(|error| panic!("commit ahead: {error:?}"));
    assert_ne!(rev(&work, "refs/heads/lane-a"), diverged);

    let rejected = backend
        .push(&id, "lane-a", ForcePush::No)
        .expect_err("a non-fast-forward push is rejected");
    let rejected_text = format!(
        "{} {}",
        rejected.message,
        rejected.hint.as_deref().unwrap_or("")
    );
    assert!(
        rejected_text.to_ascii_lowercase().contains("rejected")
            && rejected_text.contains("this lane does not"),
        "non-ff push: {rejected:?}"
    );

    let project_id = id.parse().unwrap();
    gitbutler_project::update(gitbutler_project::UpdateRequest {
        force_push_protection: Some(true),
        ..gitbutler_project::UpdateRequest::default_with_id(project_id)
    })
    .unwrap();

    let leased = backend
        .push(&id, "lane-a", ForcePush::WithLease)
        .expect_err("a protected force push is rejected when the remote moved");
    let leased_text = format!(
        "{} {}",
        leased.message,
        leased.hint.as_deref().unwrap_or("")
    );
    assert!(
        leased_text.to_ascii_lowercase().contains("rejected")
            && (leased_text.to_ascii_lowercase().contains("force")
                || leased_text.to_ascii_lowercase().contains("lease")
                || leased_text.to_ascii_lowercase().contains("overwrite")),
        "lease push: {leased:?}"
    );
    assert_eq!(
        rev(&bare, "refs/heads/lane-a"),
        diverged,
        "a rejected force push must leave the remote alone"
    );

    backend
        .push(&id, "lane-a", ForcePush::Force)
        .unwrap_or_else(|error| panic!("force push: {error:?}"));
    assert_eq!(
        rev(&bare, "refs/heads/lane-a"),
        rev(&work, "refs/heads/lane-a")
    );
    guard(root);
}

#[test]
fn askpass_broker_answers_cancels_and_times_out() {
    let isolated = isolated();
    guard(isolated.dir.path());
    write_settings();
    let backend = matching_backend();
    backend.set_askpass_timeout_for_test(Duration::from_secs(5));

    let seen = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&seen);
    let _watch = backend
        .watch_askpass(Box::new(move |request| {
            *slot.lock().unwrap() = Some(request);
        }))
        .unwrap();

    let (rx, request) = prompt_and_wait(&seen, "Password for 'https://example.test': ");
    assert_eq!(request.kind, AskpassKind::Credential);
    assert!(!format!("{request:?}").contains("s3cret"));
    let pending = backend.askpass_pending().unwrap();
    assert!(pending.iter().any(|prompt| prompt.id == request.id));
    backend
        .answer_askpass(&request.id, Some("s3cret".to_string()))
        .unwrap();
    let answer = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(answer.as_deref(), Some("s3cret"));
    assert!(backend.askpass_pending().unwrap().is_empty());

    let (rx, request) = prompt_and_wait(&seen, "Enter passphrase for key '/tmp/id_ed25519': ");
    assert_eq!(request.kind, AskpassKind::Passphrase);
    backend.answer_askpass(&request.id, None).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), None);

    backend.set_askpass_timeout_for_test(Duration::from_millis(200));
    let (rx, request) = prompt_and_wait(&seen, "Password for 'https://example.test': ");
    let answer = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(answer, None);
    assert!(backend.askpass_pending().unwrap().is_empty());
    let late = backend
        .answer_askpass(&request.id, Some("late".to_string()))
        .unwrap_err();
    assert!(late.message.contains("timed out"), "{late:?}");
    let unknown = backend.answer_askpass("not-a-prompt", None).unwrap_err();
    assert_eq!(unknown.kind, gb_bridge::BridgeErrorKind::NotFound);

    let rendered = format!("{:?}", gb_bridge::Secret::new(Some("s3cret".to_string())));
    assert_eq!(rendered, "Secret([redacted])");
    backend.set_askpass_timeout_for_test(Duration::from_secs(120));
    guard(isolated.dir.path());
}

#[test]
fn version_mismatch_refuses_remote_ops_without_writing() {
    let isolated = isolated();
    let root = isolated.dir.path();
    let (work, _bare) = setup_repo(root);
    let backend = matching_backend();
    let id = add_work(&backend, &work);
    let gitbutler = gix::open(&work).unwrap().gitbutler_storage_path().unwrap();
    let before = snapshot(root, &gitbutler);

    let mismatch = InProcessBackend::with_installed_version_for_test(Some("9.9.9")).unwrap();
    for error in [
        mismatch
            .add_project(work.to_str().unwrap())
            .expect_err("add"),
        mismatch.fetch(&id).expect_err("fetch"),
        mismatch
            .set_target(&id, "refs/remotes/origin/main", Some("origin"))
            .expect_err("target"),
        mismatch
            .push(&id, "lane-a", ForcePush::No)
            .expect_err("push"),
        mismatch.delete_project(&id).expect_err("delete"),
    ] {
        assert!(
            error.message.contains("9.9.9") && error.message.contains(PINNED_VERSION),
            "{error:?}"
        );
    }
    assert_unchanged(&before, &snapshot(root, &gitbutler));

    isolated_child_settings();
}

fn isolated_child_settings() {
    // The parent test already holds the process lock and the app-data override.
    // Remove settings and prove add does not create them.
    let settings = but_path::app_config_dir().unwrap().join("settings.json");
    let projects = but_path::app_data_dir().unwrap().join("projects.json");
    if settings.exists() {
        std::fs::remove_file(&settings).unwrap();
    }
    let backend = matching_backend();
    let error = backend
        .add_project("/tmp/does-not-matter")
        .expect_err("missing settings");
    assert!(error.message.contains("will not create"), "{error:?}");
    assert!(!settings.exists(), "settings.json was created");
    // A previous add in this test already created projects.json. The refusal
    // must not append a project for a path that never passed preflight.
    if projects.exists() {
        let text = std::fs::read_to_string(&projects).unwrap();
        assert!(
            !text.contains("does-not-matter"),
            "a refused add wrote projects.json: {text}"
        );
    }
}

fn prompt_and_wait(
    seen: &Mutex<Option<gb_bridge::AskpassRequest>>,
    prompt: &str,
) -> (mpsc::Receiver<Option<String>>, gb_bridge::AskpassRequest) {
    *seen.lock().unwrap() = None;
    let rx = prompt_on_thread(prompt);
    (rx, wait_prompt(seen))
}

fn prompt_on_thread(prompt: &str) -> mpsc::Receiver<Option<String>> {
    let prompt = prompt.to_string();
    let (tx, rx) = mpsc::channel();
    let broker = but_askpass::get_broker().expect("askpass broker");
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let answer = runtime.block_on(broker.submit_prompt(
            prompt,
            but_askpass::Context::Fetch {
                action: "fetch".to_string(),
            },
        ));
        let _ = tx.send(answer);
    });
    rx
}

fn wait_prompt(seen: &Mutex<Option<gb_bridge::AskpassRequest>>) -> gb_bridge::AskpassRequest {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(request) = seen.lock().unwrap().clone() {
            return request;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no askpass prompt arrived");
}
