//! Headless per-frame bench over a large synthetic repo (GN-132).
//!
//! Times the pure (non-GPUI) functions the `render()` path calls per frame,
//! plus the cold rebuild cost of every cached selector. Fake data only.
//! Run: `cargo test -p gb-gpui --release -- --ignored perf_bench_large --nocapture`

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use gb_bridge::{
    AssignedHunk, BranchPlace, BranchRow, BranchSummary, CiState, CommitSummary, FileChange,
    FileStatus, Review, ReviewState, SnapshotChange, StackSummary, WorkspaceSummary, WorktreeView,
};

use crate::relative;
use crate::store::{
    BranchTab, SideFocus, Store, build_diff_rows, file_change_label, snapshot_when,
};

const FILES: usize = 5_000;
const DIRS: usize = 300;
const HUNKS_PER_FILE: usize = 4;
const STACKS: usize = 10;
const BRANCHES_PER_STACK: usize = 6;
const COMMITS: usize = 3_000;
const SNAPSHOTS: usize = 2_000;
const BRANCH_ROWS: usize = 3_000;
const REVIEWS: usize = 1_000;
const SNAPSHOT_FILES: usize = 3_000;
const BIG_PATCH_LINES: usize = 20_000;
const ALL_PATCH_LINES: usize = 40_000;
const VISIBLE_ROWS: usize = 30;
const MARKED_FILES: usize = 500;
const RUNS: usize = 21;

/// ~110-character commit and branch titles, as a large history shows.
const LONG_TITLE: &str = "Implement retry with backoff for the sync worker and document the failure modes users reported";

fn bench_files() -> WorktreeView {
    let mut files = Vec::with_capacity(FILES);
    for index in 0..FILES {
        let path = format!("dir_{:03}/src/file_{index:04}.rs", index % DIRS);
        let lane = if index % 7 == 0 {
            None
        } else {
            Some(format!("stack-{}", index % STACKS))
        };
        let hunks = (0..HUNKS_PER_FILE)
            .map(|hunk| AssignedHunk {
                header: format!(
                    "@@ -{},{} +{},{} @@ fix block {hunk}",
                    hunk * 40 + 1,
                    12,
                    hunk * 40 + 1,
                    14
                ),
                lane: lane.clone(),
            })
            .collect();
        files.push(FileChange {
            path,
            old_path: None,
            status: if index % 11 == 0 {
                FileStatus::Added
            } else {
                FileStatus::Modified
            },
            additions: (index % 60) as u32,
            deletions: (index % 17) as u32,
            hunks,
        });
    }
    WorktreeView {
        files,
        changed_paths: Vec::new(),
    }
}

fn bench_workspace() -> WorkspaceSummary {
    let per_stack = COMMITS / STACKS;
    let big_lane = per_stack - (BRANCHES_PER_STACK - 1);
    let mut stacks = Vec::with_capacity(STACKS);
    let mut made = 0usize;
    for stack in 0..STACKS {
        let mut branches = Vec::with_capacity(BRANCHES_PER_STACK);
        for branch in 0..BRANCHES_PER_STACK {
            let count = if branch == 0 { big_lane } else { 1 };
            let commits = (0..count)
                .map(|index| {
                    made += 1;
                    CommitSummary {
                        sha: format!("{made:040x}"),
                        short_sha: format!("{made:07x}"),
                        title: format!("{LONG_TITLE} ({index}/{stack}/{branch})"),
                        author: "Perf Author <perf@example.com>".to_string(),
                        authored_at: 1_700_000_000 + made as i64 * 60,
                        conflicted: false,
                        pushed: made.is_multiple_of(3),
                    }
                })
                .collect();
            branches.push(BranchSummary {
                name: format!("stack-{stack}/lane-{branch}"),
                tip_sha: format!("{made:07x}"),
                tip_full: format!("{made:040x}"),
                commits,
            });
        }
        stacks.push(StackSummary { branches });
    }
    WorkspaceSummary {
        project_id: "bench-project".to_string(),
        target_branch: Some("refs/remotes/origin/main".to_string()),
        stacks,
    }
}

fn bench_branch_rows() -> Vec<BranchRow> {
    (0..BRANCH_ROWS)
        .map(|index| {
            let (place, ref_name) = match index % 3 {
                0 => (BranchPlace::Local, format!("refs/heads/topic-{index:04}")),
                1 => (
                    BranchPlace::Remote,
                    format!("refs/remotes/origin/topic-{index:04}"),
                ),
                _ => (BranchPlace::Both, format!("refs/heads/topic-{index:04}")),
            };
            BranchRow {
                name: format!("topic-{index:04}"),
                ref_name,
                place,
                remotes: if index % 3 == 0 {
                    Vec::new()
                } else {
                    vec!["origin".to_string()]
                },
                applied: index % 17 == 0,
                stack: "standalone".to_string(),
                stack_branches: vec![format!("topic-{index:04}")],
                tip_sha: format!("{index:040x}"),
                tip_short: format!("{index:07x}"),
                last_commit: format!("{LONG_TITLE} · change {index}"),
                author: "Perf <perf@example.com>".to_string(),
                updated_at_ms: Some(1_700_000_000_000),
                ahead: Some(1),
                behind: Some(0),
            }
        })
        .collect()
}

fn bench_reviews() -> Vec<Review> {
    (0..REVIEWS)
        .map(|index| Review {
            number: index as u64 + 100,
            title: format!("{LONG_TITLE} · pr {index}"),
            branch: format!("topic-{index:04}"),
            state: [ReviewState::Open, ReviewState::Draft, ReviewState::Merged][index % 3],
            ci: [CiState::None, CiState::Pending, CiState::Passing][index % 3],
            draft: index % 5 == 0,
        })
        .collect()
}

fn bench_snapshot_files() -> Vec<SnapshotChange> {
    (0..SNAPSHOT_FILES)
        .map(|index| SnapshotChange {
            path: format!("dir_{:03}/src/file_{index:04}.rs", index % DIRS),
            old_path: None,
            status: if index % 9 == 0 {
                FileStatus::Added
            } else {
                FileStatus::Modified
            },
        })
        .collect()
}

fn patch(lines: usize) -> String {
    let mut text = String::with_capacity(lines * 36);
    text.push_str("@@ -1,1 +1,1 @@\n");
    for index in 0..lines {
        text.push_str(&format!("+line {index} of the long bench diff\n"));
    }
    text
}

/// Median of `RUNS` samples, reported in milliseconds.
fn median_ms<T>(mut body: impl FnMut() -> T) -> f64 {
    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        body();
        samples.push(start.elapsed().as_micros() as u64);
    }
    samples.sort_unstable();
    samples[RUNS / 2] as f64 / 1_000.0
}

fn report(name: &str, ms: f64) {
    println!("BENCH {name}_ms={ms:.3}");
}

fn bench_store() -> (Store, Vec<String>) {
    let worktree = bench_files();
    let paths: Vec<String> = worktree
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let mut store = Store::seeded(Some(Arc::new(worktree)), Some(Arc::new(bench_workspace())));
    store.side = SideFocus::Branch {
        stack: 0,
        branch: 0,
    };
    store.set_branch_rows(bench_branch_rows());
    store.pages.tab = BranchTab::Local;
    store.set_branch_filter("topic-1".to_string());
    store.pages.snapshots = crate::store::seed_snapshots(SNAPSHOTS);
    store.pages.snapshot_files = Arc::new(bench_snapshot_files());
    store.forge.connected = true;
    store.set_reviews(bench_reviews());
    let marked_hunks = store
        .worktree
        .as_ref()
        .expect("worktree")
        .files
        .iter()
        .take(MARKED_FILES)
        .flat_map(|file| {
            file.hunks
                .iter()
                .map(|hunk| (file.path.clone(), hunk.header.clone()))
        })
        .collect();
    let marked: Vec<String> = paths.iter().take(MARKED_FILES).cloned().collect();
    store.set_marks(marked.into_iter().collect(), marked_hunks);
    let shown = paths.last().cloned().unwrap_or_default();
    store.shown = crate::store::ShownFile::Worktree { path: shown };
    store.activity.begin("bench-op", Some(paths[0].clone()));
    store.activity.advance(2_000);
    (store, paths)
}

#[test]
#[ignore = "headless bench for the hot-path baseline; run with --ignored"]
fn perf_bench_large() {
    let (store, paths) = bench_store();
    let shown_path = paths.last().cloned().unwrap_or_default();
    let mut total = 0.0;

    // Workspace page: uncommitted column.
    let worktree = Arc::clone(store.worktree.as_ref().expect("worktree"));
    let workspace = Arc::clone(store.workspace.as_ref().expect("workspace"));
    let cold_groups = median_ms(|| {
        let fresh = Store::seeded(Some(Arc::clone(&worktree)), Some(Arc::clone(&workspace)));
        black_box(fresh.change_groups());
    });
    report("workspace_change_groups_cold", cold_groups);
    let cached_slots = median_ms(|| black_box(store.file_slots()));
    report("workspace_file_slots_cached", cached_slots);
    let cold_tree = median_ms(|| {
        let fresh = Store::seeded(Some(Arc::clone(&worktree)), None);
        black_box(fresh.tree_rows());
    });
    report("workspace_tree_rows_cold", cold_tree);
    let cached_tree = median_ms(|| black_box(store.tree_rows()));
    report("workspace_tree_rows_cached", cached_tree);
    let folder_marks = median_ms(|| {
        let mut marked = 0usize;
        for dir in 0..VISIBLE_ROWS {
            if store.folder_fully_marked(&format!("dir_{dir:03}")) {
                marked += 1;
            }
        }
        black_box(marked);
    });
    report("workspace_folder_marks", folder_marks);
    let folder_marks_cold = median_ms(|| {
        let fresh = Store::seeded(Some(Arc::clone(&worktree)), None);
        black_box(fresh.folder_fully_marked("dir_000"));
    });
    report("workspace_folder_marks_cold", folder_marks_cold);
    let summary = median_ms(|| black_box(store.commit_summary()));
    report("workspace_commit_summary", summary);
    let marked_hunks = store.marked_hunks.clone();
    let marked = store.marked.clone();
    let summary_cold = median_ms(|| {
        let mut fresh = Store::seeded(Some(Arc::clone(&worktree)), Some(Arc::clone(&workspace)));
        fresh.set_marks(marked.clone(), marked_hunks.clone());
        black_box(fresh.commit_summary());
    });
    report("workspace_commit_summary_cold", summary_cold);
    let assignment = median_ms(|| {
        let mut chars = 0usize;
        for file in worktree.files.iter().take(VISIBLE_ROWS) {
            chars += file.assignment_label().len();
        }
        black_box(chars);
    });
    report("workspace_assignment_labels_window", assignment);
    let row_labels = median_ms(|| {
        let mut labels = 0usize;
        for path in paths.iter().take(VISIBLE_ROWS) {
            if let Some(label) = store.activity.row_label(path) {
                labels += label.len();
            }
        }
        black_box(labels);
    });
    report("workspace_activity_row_labels_window", row_labels);
    let heading = median_ms(|| black_box(store.changes_heading()));
    report("workspace_changes_heading", heading);
    let heading_cold = median_ms(|| {
        let fresh = Store::seeded(Some(Arc::clone(&worktree)), Some(Arc::clone(&workspace)));
        black_box(fresh.changes_heading());
    });
    report("workspace_changes_heading_cold", heading_cold);
    let lookup = median_ms(|| black_box(store.file_hunk_headers(&shown_path)));
    report("workspace_diff_file_lookup", lookup);
    let lookup_cold = median_ms(|| {
        let fresh = Store::seeded(Some(Arc::clone(&worktree)), None);
        black_box(fresh.file_hunk_headers(&shown_path));
    });
    report("workspace_diff_file_lookup_cold", lookup_cold);

    // Changes column: diff rows are built once per selection, off-thread.
    let big = patch(BIG_PATCH_LINES);
    let build_big = median_ms(|| {
        black_box(build_diff_rows(
            &shown_path,
            false,
            false,
            &big,
            &[None],
            BIG_PATCH_LINES + 8,
        ));
    });
    report("diff_rows_build_big_file", build_big);
    let all = patch(ALL_PATCH_LINES);
    let build_all = median_ms(|| {
        black_box(build_diff_rows(
            &shown_path,
            false,
            false,
            &all,
            &[None],
            ALL_PATCH_LINES + 8,
        ));
    });
    report("diff_rows_build_all", build_all);

    // Lanes: stack cards and commit rows.
    let cold_lanes = median_ms(|| {
        let fresh = Store::seeded(None, Some(Arc::clone(&workspace)));
        black_box(fresh.lane_rows());
    });
    report("lanes_lane_rows_cold", cold_lanes);
    let cached_lanes = median_ms(|| black_box(store.lane_rows()));
    report("lanes_lane_rows_cached", cached_lanes);
    let commit_rows = median_ms(|| {
        let mut text = 0usize;
        let branch = &store.workspace.as_ref().expect("workspace").stacks[0].branches[0];
        for commit in branch.commits.iter().take(VISIBLE_ROWS) {
            text += relative(commit.authored_at).len();
            text += commit.title.len();
        }
        black_box(text);
    });
    report("lanes_commit_rows_window", commit_rows);

    // Detail pane: branch focus info, rebuilt only when its inputs move.
    let info = median_ms(|| black_box(store.branch_info()));
    report("detail_branch_info", info);
    let info_cold = median_ms(|| {
        let mut fresh = Store::seeded(None, Some(Arc::clone(&workspace)));
        fresh.side = SideFocus::Branch {
            stack: 0,
            branch: 0,
        };
        black_box(fresh.branch_info());
    });
    report("detail_branch_info_cold", info_cold);

    // Branches page: filter and PR rows.
    let cold_filter = median_ms(|| {
        let mut fresh = Store::seeded(None, None);
        fresh.pages = store.pages.clone();
        black_box(fresh.branch_indexes());
    });
    report("branches_indexes_cold", cold_filter);
    let cached_filter = median_ms(|| black_box(store.branch_indexes()));
    report("branches_indexes_cached", cached_filter);
    let uncached = median_ms(|| black_box(store.pages.visible_branches()));
    report("branches_visible_branches_uncached", uncached);
    let visible_rows = median_ms(|| black_box(store.visible_branches()));
    report("branches_visible_branches_cached", visible_rows);
    let reviews = median_ms(|| black_box(store.visible_reviews()));
    report("branches_visible_reviews", reviews);
    let reviews_cold = median_ms(|| {
        let mut fresh = Store::seeded(None, None);
        fresh.set_reviews(bench_reviews());
        fresh.set_branch_filter("topic-1".to_string());
        black_box(fresh.visible_reviews());
    });
    report("branches_reviews_cold", reviews_cold);

    // History page: oplog snapshots and the snapshot file list.
    let snapshot_window = median_ms(|| {
        let mut text = 0usize;
        for shot in store.pages.snapshots.iter().skip(1_000).take(VISIBLE_ROWS) {
            text += snapshot_when(shot.created_at_ms).len();
            text += shot.title.len();
        }
        black_box(text);
    });
    report("history_snapshot_window", snapshot_window);
    let snapshot_files = median_ms(|| {
        let clone = Arc::clone(&store.pages.snapshot_files);
        let mut labels = 0usize;
        for file in clone.iter() {
            labels += file_change_label(file.status).len();
        }
        black_box((clone.len(), labels));
    });
    report("history_snapshot_files", snapshot_files);

    // Only one screen composes per frame; these are upper bounds per screen.
    for (name, value) in [
        (
            "workspace",
            cached_slots
                + cached_tree
                + folder_marks
                + summary
                + assignment
                + row_labels
                + heading
                + lookup,
        ),
        ("lanes", cached_lanes + commit_rows),
        ("detail", info),
        ("branches", cached_filter + visible_rows + reviews),
        ("history", snapshot_window + snapshot_files),
    ] {
        report(&format!("{name}_per_frame"), value);
        total += value;
    }
    report("per_frame_total", total);

    black_box((big.len(), all.len()));
}
