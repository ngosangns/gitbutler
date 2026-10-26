//! Mutation contract. Real `but_api`, temp app data, fixture repos from git.
//!
//! Every test points `E2E_TEST_APP_DATA_DIR` at a temp directory before the
//! first `but_path` call and holds a process-wide lock so the override cannot
//! leak onto the real GitButler data directory. Remotes are local bare repos.
//! This file never calls a forge, OAuth, or keychain API.
//!
//! The names in `FORBIDDEN` are the list itself. The scanner skips this file
//! and `FORGE_ALLOWLIST`. That list stays empty: the pull-request stub does
//! not call those functions, so no source path is exempt.

use std::collections::{BTreeMap, HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use but_core::RepositoryExt as _;
use gb_bridge::{
    Anchor, Backend, BridgeEvent, CommitResult, DiffRequest, HookStatus, HunkRef, InProcessBackend,
    PINNED_VERSION, SelectionSpec, WorkspaceSummary,
};
use tempfile::TempDir;

static LOCK: Mutex<()> = Mutex::new(());

/// Paths that may mention an upstream forge function. Stays empty: this client
/// never calls those functions, so nothing needs an exemption.
const FORGE_ALLOWLIST: &[&str] = &[];

const FORBIDDEN: &[&str] = &[
    "init_github_device_oauth",
    "check_github_auth_status",
    "store_github_pat",
    "store_gitlab_pat",
    "store_bitbucket_pat",
    "forge_provider",
    "forge_info",
    "list_reviews",
    "publish_review",
    "get_review",
    "update_review",
    "merge_review",
    "login_with_token",
    "secret_get_global",
    "secret_set_global",
];

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

fn git_try(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .is_ok_and(|output| output.status.success())
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

/// Same project shape as the M1 read-only fixture: commits on `main`, a
/// remote-less target (`origin/main` is a local ref), and two lanes.
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
    guard(env_root);
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

fn branch<'a>(workspace: &'a WorkspaceSummary, name: &str) -> &'a gb_bridge::BranchSummary {
    workspace
        .stacks
        .iter()
        .flat_map(|stack| &stack.branches)
        .find(|branch| branch.name == name)
        .unwrap_or_else(|| panic!("no lane {name} in {workspace:?}"))
}

fn ref_sha(work: &Path, name: &str) -> String {
    git(work, &["rev-parse", &format!("refs/heads/{name}")])
        .trim()
        .to_string()
}

fn show_file(work: &Path, sha: &str, path: &str) -> String {
    git(work, &["show", &format!("{sha}:{path}")])
}

fn show_file_ok(work: &Path, sha: &str, path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["show", &format!("{sha}:{path}")])
        .current_dir(work)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

fn log_text(work: &Path, rev: &str) -> String {
    git(work, &["log", "-1", "--format=%B", rev])
}

fn log_subjects(work: &Path, rev: &str) -> Vec<String> {
    git(work, &["log", "--format=%s", rev])
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_hunks(patch: &str) -> Vec<HunkRef> {
    patch
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("@@ -")?;
            let (old, rest) = rest.split_once(" +")?;
            let new = rest.split_once(" @@")?.0;
            let (old_start, old_lines) = parse_range(old)?;
            let (new_start, new_lines) = parse_range(new)?;
            Some(HunkRef {
                old_start,
                old_lines,
                new_start,
                new_lines,
            })
        })
        .collect()
}

fn parse_range(text: &str) -> Option<(u32, u32)> {
    let text = text.trim();
    if let Some((start, lines)) = text.split_once(',') {
        Some((start.parse().ok()?, lines.parse().ok()?))
    } else {
        Some((text.parse().ok()?, 1))
    }
}

fn wide_body(first: &str, last: &str) -> String {
    const COUNT: usize = 32;
    let mut out = String::new();
    for index in 0..COUNT {
        let value = if index == 0 {
            first
        } else if index + 1 == COUNT {
            last
        } else {
            "base"
        };
        out.push_str(&format!("fn keep_{index}() {{ return \"{value}\"; }}\n"));
    }
    out
}

fn worktree_file(backend: &InProcessBackend, id: &str, path: &str) -> gb_bridge::FileChange {
    backend
        .worktree(id)
        .unwrap()
        .files
        .into_iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the worktree"))
}

fn assignment_rows(gitbutler: &Path) -> Vec<(String, String)> {
    let db = gitbutler.join("but.sqlite");
    let connection =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap_or_else(|error| panic!("open {}: {error}", db.display()));
    let mut statement = connection
        .prepare("SELECT path, branch_ref FROM hunk_assignments")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            let path: String = row.get(0)?;
            let branch: Option<Vec<u8>> = row.get(1)?;
            Ok((
                path,
                branch
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default(),
            ))
        })
        .unwrap();
    rows.map(|row| row.unwrap()).collect()
}

fn commit_ok(result: CommitResult, what: &str) -> String {
    assert!(
        result.rejected.is_empty(),
        "{what} rejected {:?}",
        result.rejected
    );
    result
        .new_sha
        .unwrap_or_else(|| panic!("{what} produced no commit: {:?}", result.workspace))
}

#[test]
fn assign_moves_between_lanes_and_hooks_report_data() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let backend = matching_backend();
    let (tx, rx) = mpsc::channel();
    let _watch = backend
        .watch(
            &fixture.id,
            Box::new(move |event: BridgeEvent| {
                let _ = tx.send(event.name().to_string());
            }),
        )
        .unwrap();

    let created = backend.create_lane(&fixture.id, "lane-c").unwrap();
    assert_eq!(created.name, "lane-c");
    assert!(
        created
            .workspace
            .stacks
            .iter()
            .any(|stack| stack.names().any(|name| name == "lane-c")),
        "{:?}",
        created.workspace
    );
    // An empty lane's git ref sits on the workspace base. `tip_full` stays
    // empty until the lane has a commit of its own, so only the ref is checked.
    let _lane_c = ref_sha(&fixture.work, "lane-c");

    std::fs::write(fixture.work.join("note.txt"), "note\n").unwrap();
    let file = worktree_file(&backend, &fixture.id, "note.txt");
    assert!(
        file.lanes().is_empty(),
        "new file starts unassigned: {file:?}"
    );

    backend
        .assign(
            &fixture.id,
            &[SelectionSpec::whole("note.txt")],
            Some("lane-a"),
        )
        .unwrap();
    let file = worktree_file(&backend, &fixture.id, "note.txt");
    assert_eq!(file.lanes(), ["lane-a"]);
    let rows = assignment_rows(&fixture.gitbutler);
    assert!(
        rows.iter()
            .any(|(path, branch)| path == "note.txt" && branch.contains("lane-a")),
        "db rows after assign to lane-a: {rows:?}"
    );

    backend
        .assign(
            &fixture.id,
            &[SelectionSpec::whole("note.txt")],
            Some("lane-b"),
        )
        .unwrap();
    let file = worktree_file(&backend, &fixture.id, "note.txt");
    assert_eq!(file.lanes(), ["lane-b"]);
    let rows = assignment_rows(&fixture.gitbutler);
    assert!(
        rows.iter()
            .any(|(path, branch)| path == "note.txt" && branch.contains("lane-b")),
        "db rows after assign to lane-b: {rows:?}"
    );
    assert!(
        rows.iter()
            .filter(|(path, _)| path == "note.txt")
            .all(|(_, branch)| !branch.contains("lane-a")),
        "lane-a assignment should be gone: {rows:?}"
    );

    backend
        .assign(&fixture.id, &[SelectionSpec::whole("note.txt")], None)
        .unwrap();
    let file = worktree_file(&backend, &fixture.id, "note.txt");
    assert!(file.lanes().is_empty(), "{file:?}");
    let rows = assignment_rows(&fixture.gitbutler);
    assert!(
        rows.iter()
            .filter(|(path, _)| path == "note.txt")
            .all(|(_, branch)| branch.is_empty()),
        "unassign leaves no lane in the db: {rows:?}"
    );

    let before = ref_sha(&fixture.work, "lane-a");
    let refused = backend
        .commit_create(
            &fixture.id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("note.txt")],
            "   ",
        )
        .unwrap_err();
    assert!(
        refused.message.contains("commit message"),
        "{}",
        refused.message
    );
    assert_eq!(ref_sha(&fixture.work, "lane-a"), before);

    // GitButler installs a managed pre-commit hook when the project is set up.
    // That hook exits 0, or exits 1 with GITBUTLER_ERROR on `gitbutler/workspace`,
    // and `pre_commit_hook_diffspecs` maps both of those to Success. post-commit
    // and the message hook are not installed, so they stay NotConfigured.
    let pre = backend
        .pre_commit_hook(&fixture.id, &[SelectionSpec::whole("note.txt")])
        .unwrap();
    let post = backend.post_commit_hook(&fixture.id).unwrap();
    let message = backend.message_hook(&fixture.id, "hello").unwrap();
    assert_eq!(pre.status, HookStatus::Success);
    assert_eq!(post.status, HookStatus::NotConfigured);
    assert_eq!(message.status, HookStatus::NotConfigured);

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(name) => {
                if name == "workspaceActivity" {
                    return;
                }
                seen.push(name);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    panic!("mutation did not emit a refresh event, saw {seen:?}");
}

#[test]
fn commit_graph_matches_git_and_undo_restores_a_commit() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let backend = matching_backend();
    let work = &fixture.work;
    let id = &fixture.id;

    std::fs::write(work.join("wide.rs"), wide_body("base", "base")).unwrap();
    let wide = commit_ok(
        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::whole("wide.rs")],
                "add wide",
            )
            .unwrap(),
        "add wide",
    );
    assert_eq!(ref_sha(work, "lane-a"), wide);
    assert_eq!(
        branch(&backend.open_project(id).unwrap(), "lane-a").tip_full,
        wide
    );

    std::fs::write(work.join("wide.rs"), wide_body("ALPHA", "OMEGA")).unwrap();
    let patch = backend
        .file_diff(
            id,
            DiffRequest::Worktree {
                path: "wide.rs".to_string(),
            },
        )
        .unwrap();
    let hunks = parse_hunks(&patch.patch);
    assert!(
        hunks.len() >= 2,
        "expected two hunks, got {hunks:?}\n{}",
        patch.patch
    );
    let alpha = *hunks.iter().min_by_key(|hunk| hunk.new_start).unwrap();
    let partial = commit_ok(
        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::hunks("wide.rs", vec![alpha])],
                "commit alpha",
            )
            .unwrap(),
        "commit alpha",
    );
    let committed = show_file(work, &partial, "wide.rs");
    assert!(committed.contains("ALPHA"), "{committed}");
    assert!(!committed.contains("OMEGA"), "{committed}");
    let disk = std::fs::read_to_string(work.join("wide.rs")).unwrap();
    assert!(disk.contains("ALPHA") && disk.contains("OMEGA"), "{disk}");
    let remaining = git(work, &["diff", &partial, "--", "wide.rs"]);
    assert!(remaining.contains("OMEGA"), "{remaining}");
    assert_eq!(ref_sha(work, "lane-a"), partial);
    assert_eq!(
        branch(&backend.open_project(id).unwrap(), "lane-a").tip_full,
        partial
    );

    let listed = backend.snapshots(id, 20).unwrap();
    assert!(
        listed.iter().any(|snapshot| {
            snapshot.operation == "CreateCommit"
                && snapshot.title == "Created commit"
                && !snapshot.sha.is_empty()
                && snapshot.created_at_ms > 0
        }),
        "snapshots: {listed:?}"
    );
    let undone = backend.undo_last(id).unwrap();
    assert!(!undone.restored_sha.is_empty());
    assert_eq!(
        ref_sha(work, "lane-a"),
        wide,
        "undo restores the pre-commit tip"
    );
    assert_eq!(branch(&undone.workspace, "lane-a").tip_full, wide);
    let disk = std::fs::read_to_string(work.join("wide.rs")).unwrap();
    assert!(
        disk.contains("ALPHA") && disk.contains("OMEGA"),
        "undo keeps the uncommitted edits\n{disk}"
    );

    let partial = commit_ok(
        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::hunks("wide.rs", vec![alpha])],
                "commit alpha",
            )
            .unwrap(),
        "commit alpha again",
    );
    let patch = backend
        .file_diff(
            id,
            DiffRequest::Worktree {
                path: "wide.rs".to_string(),
            },
        )
        .unwrap();
    let omega = parse_hunks(&patch.patch)
        .into_iter()
        .max_by_key(|hunk| hunk.new_start)
        .expect("omega hunk");
    let amended = commit_ok(
        backend
            .amend(
                id,
                &partial,
                &[SelectionSpec::hunks("wide.rs", vec![omega])],
            )
            .unwrap(),
        "amend omega",
    );
    let committed = show_file(work, &amended, "wide.rs");
    assert!(
        committed.contains("ALPHA") && committed.contains("OMEGA"),
        "{committed}"
    );
    let remaining = git(work, &["diff", &amended, "--", "wide.rs"]);
    assert!(!remaining.contains("OMEGA"), "{remaining}");

    let current = std::fs::read_to_string(work.join("wide.rs")).unwrap();
    let edited = current.replacen(
        "fn keep_16() { return \"base\"; }",
        "fn keep_16() { return \"MIDDLE\"; }",
        1,
    );
    assert_ne!(edited, current, "keep_16 was not in the committed file");
    std::fs::write(work.join("wide.rs"), edited).unwrap();
    let middle = commit_ok(
        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::whole("wide.rs")],
                "middle edit",
            )
            .unwrap(),
        "middle edit",
    );
    let moved = backend
        .move_changes(id, &middle, &amended, &[SelectionSpec::whole("wide.rs")])
        .unwrap();
    let folded = branch(&moved.workspace, "lane-a")
        .commits
        .iter()
        .map(|commit| commit.sha.clone())
        .find(|sha| {
            show_file_ok(work, sha, "wide.rs").is_some_and(|blob| {
                blob.contains("ALPHA") && blob.contains("OMEGA") && blob.contains("MIDDLE")
            })
        })
        .unwrap_or_else(|| panic!("no commit holds the moved change: {:?}", moved.workspace));
    assert_eq!(folded.len(), 40, "{folded}");

    std::fs::write(work.join("extra.txt"), "extra\n").unwrap();
    std::fs::write(work.join("other.txt"), "other\n").unwrap();
    let pair = commit_ok(
        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[
                    SelectionSpec::whole("extra.txt"),
                    SelectionSpec::whole("other.txt"),
                ],
                "second marker",
            )
            .unwrap(),
        "second marker",
    );
    let extracted = backend
        .uncommit_changes(
            id,
            &pair,
            &[SelectionSpec::whole("other.txt")],
            Some("lane-a"),
        )
        .unwrap();
    assert!(
        std::fs::read_to_string(work.join("other.txt"))
            .unwrap()
            .contains("other"),
        "uncommit puts the file back in the worktree"
    );
    let pair_tip = branch(&extracted.workspace, "lane-a")
        .commits
        .iter()
        .find(|commit| commit.title.contains("second marker"))
        .unwrap_or_else(|| panic!("pair missing after uncommit: {:?}", extracted.workspace))
        .sha
        .clone();
    assert!(
        show_file(work, &pair_tip, "extra.txt").contains("extra"),
        "the other file stays in the commit"
    );
    assert!(
        !git_try(work, &["cat-file", "-e", &format!("{pair_tip}:other.txt")]),
        "other.txt should have left the commit"
    );

    let reworded = commit_ok(
        backend
            .reword(id, &pair_tip, "second marker renamed")
            .unwrap(),
        "reword",
    );
    assert!(log_text(work, &reworded).contains("second marker renamed"));
    assert_eq!(
        branch(&backend.open_project(id).unwrap(), "lane-a").tip_full,
        reworded
    );

    std::fs::write(work.join("third.txt"), "third\n").unwrap();
    let third = commit_ok(
        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::whole("third.txt")],
                "third marker",
            )
            .unwrap(),
        "third marker",
    );
    let squashed = commit_ok(backend.squash(id, &[third], &reworded).unwrap(), "squash");
    let message = log_text(work, &squashed);
    assert!(message.contains("second marker renamed"), "{message}");
    assert!(message.contains("third marker"), "{message}");
    assert!(show_file(work, &squashed, "third.txt").contains("third"));
    assert_eq!(ref_sha(work, "lane-a"), squashed);

    let on_b = backend
        .move_commit(
            id,
            std::slice::from_ref(&squashed),
            &Anchor::Lane("lane-b".to_string()),
        )
        .unwrap();
    let lane_b = ref_sha(work, "lane-b");
    assert_eq!(branch(&on_b.workspace, "lane-b").tip_full, lane_b);
    let moved_message = log_text(work, &lane_b);
    assert!(
        moved_message.contains("second marker renamed"),
        "{moved_message}"
    );
    assert!(moved_message.contains("third marker"), "{moved_message}");
    assert!(
        !log_subjects(work, "refs/heads/lane-a")
            .iter()
            .any(|subject| subject.contains("second marker")),
        "lane-a after the move: {:?}",
        log_subjects(work, "refs/heads/lane-a")
    );

    let back = backend
        .move_commit(
            id,
            std::slice::from_ref(&lane_b),
            &Anchor::Lane("lane-a".to_string()),
        )
        .unwrap();
    let lane_a = ref_sha(work, "lane-a");
    assert_eq!(branch(&back.workspace, "lane-a").tip_full, lane_a);
    assert!(log_text(work, &lane_a).contains("third marker"));
    assert_eq!(
        log_subjects(work, "refs/heads/lane-b")[0],
        "commit lane-b",
        "{:?}",
        log_subjects(work, "refs/heads/lane-b")
    );

    let uncommitted = backend
        .uncommit(id, std::slice::from_ref(&lane_a), Some("lane-b"))
        .unwrap();
    assert!(
        std::fs::read_to_string(work.join("third.txt"))
            .unwrap()
            .contains("third"),
        "uncommit restores third.txt"
    );
    assert!(
        !log_subjects(work, "refs/heads/lane-a")
            .iter()
            .any(|subject| subject.contains("third marker")),
        "{:?}",
        log_subjects(work, "refs/heads/lane-a")
    );
    assert_eq!(
        branch(&uncommitted.workspace, "lane-a").tip_full,
        ref_sha(work, "lane-a")
    );
    let third_file = worktree_file(&backend, id, "third.txt");
    assert!(
        third_file.lanes().contains(&"lane-b"),
        "uncommit assigns the restored file: {third_file:?}"
    );

    let before_blank = ref_sha(work, "lane-b");
    let blank = commit_ok(
        backend
            .insert_blank(id, &Anchor::Lane("lane-b".to_string()))
            .unwrap(),
        "blank",
    );
    assert_eq!(ref_sha(work, "lane-b"), blank);
    assert_eq!(
        git(work, &["rev-parse", &format!("{blank}^")]).trim(),
        before_blank
    );
    assert_eq!(
        git(work, &["rev-parse", &format!("{blank}^{{tree}}")]).trim(),
        git(work, &["rev-parse", &format!("{before_blank}^{{tree}}")]).trim(),
        "a blank commit keeps its parent's tree"
    );
    assert!(
        log_text(work, &blank).trim().is_empty(),
        "blank message is empty: {:?}",
        log_text(work, &blank)
    );

    let parked = backend
        .move_commit(
            id,
            std::slice::from_ref(&blank),
            &Anchor::Lane("lane-a".to_string()),
        )
        .unwrap();
    let lane_a = ref_sha(work, "lane-a");
    assert_eq!(branch(&parked.workspace, "lane-a").tip_full, lane_a);
    assert!(
        log_text(work, &lane_a).trim().is_empty(),
        "the blank commit is lane-a's tip: {:?}",
        log_text(work, &lane_a)
    );
    assert_eq!(ref_sha(work, "lane-b"), before_blank);
}

#[test]
fn discard_drops_a_file_and_a_hunk_and_undo_restores_both() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let backend = matching_backend();
    let work = &fixture.work;
    let id = &fixture.id;

    // `discard_worktree_changes` records a DiscardChanges snapshot before it
    // edits the worktree. Undo restores that snapshot, so a discarded file and
    // a discarded hunk both come back. This is the guarantee this test locks.
    std::fs::write(work.join("gone.txt"), "bye\n").unwrap();
    let discarded = backend
        .discard(id, &[SelectionSpec::whole("gone.txt")])
        .unwrap();
    assert!(discarded.refused.is_empty(), "{:?}", discarded.refused);
    assert!(
        !work.join("gone.txt").exists(),
        "discard removes the new file"
    );
    assert!(
        backend
            .worktree(id)
            .unwrap()
            .files
            .iter()
            .all(|file| file.path != "gone.txt")
    );
    backend.undo_last(id).unwrap();
    assert_eq!(
        std::fs::read_to_string(work.join("gone.txt")).unwrap(),
        "bye\n",
        "undo after discard restores the file"
    );

    let mut seeded = String::new();
    for index in 0..32 {
        seeded.push_str(&format!("line {index}\n"));
    }
    std::fs::write(work.join("hunks.txt"), &seeded).unwrap();
    let seeded_sha = commit_ok(
        backend
            .commit_create(
                id,
                &Anchor::Lane("lane-a".to_string()),
                &[SelectionSpec::whole("hunks.txt")],
                "seed hunks",
            )
            .unwrap(),
        "seed hunks",
    );
    let mut edited = seeded.clone();
    edited = edited.replacen("line 0\n", "ALPHA-LINE\n", 1);
    edited = edited.replacen("line 31\n", "OMEGA-LINE\n", 1);
    std::fs::write(work.join("hunks.txt"), &edited).unwrap();
    let patch = backend
        .file_diff(
            id,
            DiffRequest::Worktree {
                path: "hunks.txt".to_string(),
            },
        )
        .unwrap();
    let hunks = parse_hunks(&patch.patch);
    assert!(hunks.len() >= 2, "{hunks:?}\n{}", patch.patch);
    let omega = *hunks.iter().max_by_key(|hunk| hunk.new_start).unwrap();
    let discarded = backend
        .discard(id, &[SelectionSpec::hunks("hunks.txt", vec![omega])])
        .unwrap();
    assert!(discarded.refused.is_empty(), "{:?}", discarded.refused);
    let disk = std::fs::read_to_string(work.join("hunks.txt")).unwrap();
    assert!(disk.contains("ALPHA-LINE"), "{disk}");
    assert!(!disk.contains("OMEGA-LINE"), "{disk}");
    assert!(disk.contains("line 31"), "{disk}");

    backend.undo_last(id).unwrap();
    let disk = std::fs::read_to_string(work.join("hunks.txt")).unwrap();
    assert!(
        disk.contains("ALPHA-LINE") && disk.contains("OMEGA-LINE"),
        "undo after a hunk discard restores that hunk\n{disk}"
    );
    assert_eq!(
        ref_sha(work, "lane-a"),
        seeded_sha,
        "the hunk discard does not rewrite the commit"
    );
}

#[test]
fn mutations_refuse_a_mismatched_or_unmigrated_project() {
    let isolated = isolated();
    let fixture = setup_fixture(isolated.dir.path());
    let before = snapshot(isolated.dir.path(), &fixture.gitbutler);

    let mismatch = InProcessBackend::with_installed_version_for_test(Some("9.9.9")).unwrap();
    let error = mismatch.create_lane(&fixture.id, "lane-z").unwrap_err();
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
        .create_lane(&fixture.id, "lane-z")
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
        .commit_create(
            &fixture.id,
            &Anchor::Lane("lane-a".to_string()),
            &[SelectionSpec::whole("README.md")],
            "should not land",
        )
        .unwrap_err();
    assert!(
        error.message.contains("will not create"),
        "{}",
        error.message
    );
    assert!(!settings.exists(), "settings.json was recreated");
}

#[test]
fn no_forge_or_oauth_names_in_these_crates() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("crates");
    let mut stack = vec![root.clone()];
    let mut hits = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|error| {
            panic!("{}: {error}", dir.display());
        });
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path
                .file_name()
                .is_some_and(|name| name == "target" || name == ".git")
            {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if FORGE_ALLOWLIST.iter().any(|allowed| relative == *allowed) {
                continue;
            }
            let in_source = relative.contains("/src/")
                || relative.contains("/tests/")
                || relative.starts_with("src/")
                || relative.starts_with("tests/");
            if !in_source || path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            if relative == "gb-bridge/tests/mutations.rs" {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                for name in FORBIDDEN {
                    if line.contains(name) {
                        hits.push(format!("{}:{}: {name}", relative, index + 1));
                    }
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "forge or oauth names are not part of this slice:\n{}",
        hits.join("\n")
    );
}
