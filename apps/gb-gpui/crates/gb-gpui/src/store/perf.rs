//! Frame and selector measurements. Fake data only.
//!
//! The headless numbers compare the work one frame used to do (clone every
//! row, regroup every call, one refresh per watcher event) with the work it
//! does now (a cached selector, a visible window, a coalesced refresh).
//! The dev command `perf` adds real `render()` times on top of this.

use std::sync::Arc;
use std::time::Instant;

use gb_bridge::{
    AssignedHunk, BranchPlace, BranchRow, BranchSummary, BridgeEvent, CiState, CommitSummary,
    FileChange, FileStatus, Review, ReviewState, SnapshotChange, SnapshotInfo, StackSummary,
    WorkspaceSummary, WorktreeView,
};

use super::{Store, build_diff_rows, group_files, lane_order};

/// Quiet period before a watcher burst may start a workspace refresh.
pub const REFRESH_QUIET_MS: u64 = 200;

/// Rows a diff frame is allowed to paint before the user asks for more.
pub const DIFF_PAGE: usize = 400;

/// Extra rows drawn above and below the viewport.
pub const OVERSCAN: usize = 6;

/// One refresh gate. A burst notes events; `poll` starts at most one refresh
/// after the quiet period; `finish` allows a single trailing refresh.
#[derive(Debug, Clone)]
pub struct RefreshGate {
    delay_ms: u64,
    dirty_at: Option<u64>,
    in_flight: bool,
    trailing: bool,
    started: u32,
}

impl RefreshGate {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            dirty_at: None,
            in_flight: false,
            trailing: false,
            started: 0,
        }
    }

    pub fn note(&mut self, now_ms: u64) {
        if self.in_flight {
            self.trailing = true;
            return;
        }
        if self.dirty_at.is_none() {
            self.dirty_at = Some(now_ms);
        }
    }

    /// Start a refresh when the quiet period has elapsed and none is running.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if self.in_flight {
            return false;
        }
        let Some(at) = self.dirty_at else {
            return false;
        };
        if now_ms.saturating_sub(at) < self.delay_ms {
            return false;
        }
        self.dirty_at = None;
        self.in_flight = true;
        self.started = self.started.saturating_add(1);
        true
    }

    pub fn finish(&mut self, now_ms: u64) {
        self.in_flight = false;
        if self.trailing {
            self.trailing = false;
            self.dirty_at = Some(now_ms.saturating_sub(self.delay_ms));
        }
    }

    pub fn cancel(&mut self) {
        self.in_flight = false;
        self.trailing = false;
        self.dirty_at = None;
    }

    pub fn started(&self) -> u32 {
        self.started
    }

    #[cfg(any(test, feature = "devtools"))]
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }
}

impl Default for RefreshGate {
    fn default() -> Self {
        Self::new(REFRESH_QUIET_MS)
    }
}

/// `scroll_px` is how far the list has moved. The range is clamped to `len`.
pub fn visible_window(
    len: usize,
    scroll_px: f32,
    viewport_px: f32,
    item_px: f32,
    overscan: usize,
) -> std::ops::Range<usize> {
    if len == 0 || item_px <= 0.0 || viewport_px <= 0.0 {
        return 0..0;
    }
    let scroll = scroll_px.max(0.0);
    let first = (scroll / item_px).floor() as usize;
    let shown = (viewport_px / item_px).ceil() as usize;
    let start = first.saturating_sub(overscan).min(len);
    let end = first
        .saturating_add(shown)
        .saturating_add(overscan)
        .min(len);
    start..end.max(start)
}

pub fn seed_files(count: usize) -> WorktreeView {
    let mut files = Vec::with_capacity(count);
    for index in 0..count {
        let lane = if index % 5 == 0 {
            None
        } else {
            Some(format!("lane-{}", index % 4))
        };
        files.push(FileChange {
            path: format!("src/file_{index:04}.rs"),
            old_path: None,
            status: if index % 11 == 0 {
                FileStatus::Added
            } else {
                FileStatus::Modified
            },
            additions: (index % 20) as u32,
            deletions: (index % 7) as u32,
            hunks: vec![AssignedHunk {
                header: format!("@@ -{index},1 +{index},1 @@"),
                lane,
            }],
        });
    }
    WorktreeView {
        files,
        changed_paths: Vec::new(),
    }
}

pub fn seed_workspace(commits: usize) -> WorkspaceSummary {
    let mut list = Vec::with_capacity(commits);
    for index in 0..commits {
        list.push(CommitSummary {
            sha: format!("{index:040x}"),
            short_sha: format!("{index:07x}"),
            title: format!("Commit {index}"),
            author: "Perf <perf@example.com>".to_string(),
            authored_at: 1_700_000_000 + index as i64,
            conflicted: false,
            pushed: index % 3 == 0,
        });
    }
    let tip = list.first().cloned().unwrap_or(CommitSummary {
        sha: "0".repeat(40),
        short_sha: "0000000".to_string(),
        title: "empty".to_string(),
        author: String::new(),
        authored_at: 0,
        conflicted: false,
        pushed: false,
    });
    WorkspaceSummary {
        project_id: "perf-project".to_string(),
        target_branch: Some("refs/remotes/origin/main".to_string()),
        stacks: vec![StackSummary {
            branches: vec![BranchSummary {
                name: "perf-lane".to_string(),
                tip_sha: tip.short_sha.clone(),
                tip_full: tip.sha.clone(),
                commits: list,
            }],
        }],
    }
}

pub fn seed_diff(lines: usize) -> Vec<super::DiffRow> {
    let mut patch = String::from("@@ -1,1 +1,1 @@\n");
    for index in 0..lines {
        patch.push_str(&format!("+line {index} of the long diff\n"));
    }
    build_diff_rows("src/long.diff", false, false, &patch, &[None], lines + 8)
}

pub fn seed_snapshots(count: usize) -> Vec<SnapshotInfo> {
    (0..count)
        .map(|index| SnapshotInfo {
            sha: format!("{index:040x}"),
            created_at_ms: 1_700_000_000_000 + index as i128,
            operation: "CreateCommit".to_string(),
            title: format!("Snapshot {index}"),
            restorable: true,
        })
        .collect()
}

pub fn seed_branches(count: usize) -> Vec<BranchRow> {
    (0..count)
        .map(|index| BranchRow {
            name: format!("topic-{index:04}"),
            ref_name: format!("refs/heads/topic-{index:04}"),
            place: BranchPlace::Local,
            remotes: Vec::new(),
            applied: index % 17 == 0,
            stack: "standalone".to_string(),
            stack_branches: vec![format!("topic-{index:04}")],
            tip_sha: format!("{index:040x}"),
            tip_short: format!("{index:07x}"),
            last_commit: format!("Change {index}"),
            author: "Perf <perf@example.com>".to_string(),
            updated_at_ms: Some(1_700_000_000_000),
            ahead: Some(1),
            behind: Some(0),
        })
        .collect()
}

pub fn seed_git_event() -> BridgeEvent {
    BridgeEvent::GitActivity {
        head_sha: "abc".to_string(),
    }
}

/// ~110-character commit and branch titles, as a large history shows.
const LARGE_TITLE: &str = "Implement retry with backoff for the sync worker and document the failure modes users reported";

const LARGE_FILES: usize = 5_000;
const LARGE_DIRS: usize = 300;
const LARGE_STACKS: usize = 10;
const LARGE_BRANCHES_PER_STACK: usize = 6;
const LARGE_COMMITS: usize = 3_000;
const LARGE_BRANCH_ROWS: usize = 3_000;
const LARGE_REVIEWS: usize = 1_000;
const LARGE_SNAPSHOT_FILES: usize = 3_000;

/// A 5,000-file worktree across ~300 nested directories, four hunks per file.
/// Mirrors `bench_files` in `src/bench.rs` for the real-window scenarios.
pub fn seed_large_files() -> WorktreeView {
    let mut files = Vec::with_capacity(LARGE_FILES);
    for index in 0..LARGE_FILES {
        let path = format!("dir_{:03}/src/file_{index:04}.rs", index % LARGE_DIRS);
        let lane = if index % 7 == 0 {
            None
        } else {
            Some(format!("stack-{}", index % LARGE_STACKS))
        };
        let hunks = (0..4)
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

/// Ten stacks of six branches over 3,000 commits, long titles, one big lane
/// per stack. Mirrors `bench_workspace` in `src/bench.rs`.
pub fn seed_large_workspace() -> WorkspaceSummary {
    large_workspace(LARGE_COMMITS / LARGE_STACKS - (LARGE_BRANCHES_PER_STACK - 1))
}

/// The branch-detail shape: one branch of `LARGE_COMMITS` long-title commits
/// on a ten-stack workspace, every other branch a single commit.
pub fn seed_detail_workspace() -> WorkspaceSummary {
    let mut workspace = large_workspace(1);
    let commits = (0..LARGE_COMMITS)
        .map(|index| CommitSummary {
            sha: format!("{index:040x}"),
            short_sha: format!("{index:07x}"),
            title: format!("{LARGE_TITLE} ({index})"),
            author: "Perf Author <perf@example.com>".to_string(),
            authored_at: 1_700_000_000 + index as i64 * 60,
            conflicted: false,
            pushed: index.is_multiple_of(3),
        })
        .collect();
    workspace.stacks[0].branches[0].commits = commits;
    workspace.stacks[0].branches[0].tip_sha = format!("{LARGE_COMMITS:07x}");
    workspace.stacks[0].branches[0].tip_full = format!("{LARGE_COMMITS:040x}");
    workspace
}

fn large_workspace(big_lane: usize) -> WorkspaceSummary {
    let mut made = 0usize;
    let mut stacks = Vec::with_capacity(LARGE_STACKS);
    for stack in 0..LARGE_STACKS {
        let mut branches = Vec::with_capacity(LARGE_BRANCHES_PER_STACK);
        for branch in 0..LARGE_BRANCHES_PER_STACK {
            let count = if branch == 0 { big_lane } else { 1 };
            let commits = (0..count)
                .map(|index| {
                    made += 1;
                    CommitSummary {
                        sha: format!("{made:040x}"),
                        short_sha: format!("{made:07x}"),
                        title: format!("{LARGE_TITLE} ({index}/{stack}/{branch})"),
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
        project_id: "perf-project".to_string(),
        target_branch: Some("refs/remotes/origin/main".to_string()),
        stacks,
    }
}

/// 3,000 branch rows split over local, remote, and both. Mirrors
/// `bench_branch_rows` in `src/bench.rs`.
pub fn seed_large_branches() -> Vec<BranchRow> {
    (0..LARGE_BRANCH_ROWS)
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
                last_commit: format!("{LARGE_TITLE} · change {index}"),
                author: "Perf <perf@example.com>".to_string(),
                updated_at_ms: Some(1_700_000_000_000),
                ahead: Some(1),
                behind: Some(0),
            }
        })
        .collect()
}

/// 1,000 forge reviews. Mirrors `bench_reviews` in `src/bench.rs`.
pub fn seed_large_reviews() -> Vec<Review> {
    (0..LARGE_REVIEWS)
        .map(|index| Review {
            number: index as u64 + 100,
            title: format!("{LARGE_TITLE} · pr {index}"),
            branch: format!("topic-{index:04}"),
            state: [ReviewState::Open, ReviewState::Draft, ReviewState::Merged][index % 3],
            ci: [CiState::None, CiState::Pending, CiState::Passing][index % 3],
            draft: index % 5 == 0,
        })
        .collect()
}

/// A selected snapshot's 3,000 changed files. Mirrors `bench_snapshot_files`.
pub fn seed_large_snapshot_files() -> Vec<SnapshotChange> {
    (0..LARGE_SNAPSHOT_FILES)
        .map(|index| SnapshotChange {
            path: format!("dir_{:03}/src/file_{index:04}.rs", index % LARGE_DIRS),
            old_path: None,
            status: if index % 9 == 0 {
                FileStatus::Added
            } else {
                FileStatus::Modified
            },
        })
        .collect()
}

/// How many refreshes a gate starts for `count` events one millisecond apart.
pub fn coalesced_refreshes(count: usize, delay_ms: u64) -> u32 {
    let mut gate = RefreshGate::new(delay_ms);
    let count = count as u64;
    for now in 0..count {
        gate.note(now);
        let _ = gate.poll(now);
    }
    gate.finish(count);
    let _ = gate.poll(count);
    gate.started()
}

fn median(mut samples: Vec<u64>) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn sink<T>(value: T) {
    std::hint::black_box(value);
}

fn time_us(mut body: impl FnMut()) -> u64 {
    let start = Instant::now();
    body();
    start.elapsed().as_micros() as u64
}

fn median_us(mut body: impl FnMut(), times: usize) -> u64 {
    median((0..times).map(|_| time_us(&mut body)).collect())
}

fn materialize_files(store: &Store, range: std::ops::Range<usize>) -> usize {
    let Some(worktree) = store.worktree.as_ref() else {
        return 0;
    };
    let mut labels = 0usize;
    for index in range {
        let Some(file) = worktree.files.get(index) else {
            continue;
        };
        labels = labels.saturating_add(file.path.len());
        labels = labels.saturating_add(file.assignment_label().len());
    }
    labels
}

fn header_scan(rows: &[super::DiffRow], end: usize) -> usize {
    rows.iter()
        .take(end)
        .filter(|row| row.hunk.is_some())
        .count()
}

/// Headless before/after for one machine. `before_*` is the uncached full scan.
/// `after_*` is the cached selector or the visible window.
pub fn measure() -> serde_json::Value {
    let files = seed_files(2_000);
    let workspace = seed_workspace(2_000);
    let order = lane_order(Some(&workspace));
    let uncached_group = median_us(|| sink(group_files(&files.files, &order)), 5);

    let mut store = Store {
        worktree: Some(Arc::new(files)),
        workspace: Some(Arc::new(workspace)),
        ..Store::default()
    };
    let _ = store.change_groups();
    let cached_group = median_us(|| sink(store.change_groups()), 5);
    let cached_lanes = {
        let _ = store.lane_rows();
        median_us(|| sink(store.lane_rows()), 5)
    };
    let uncached_lanes = median_us(
        || {
            let fresh = Store {
                workspace: store.workspace.clone(),
                ..Store::default()
            };
            sink(fresh.lane_rows());
        },
        3,
    );

    let deep_clone = median_us(
        || {
            sink(store.worktree.as_deref().cloned());
        },
        3,
    );
    let arc_clone = median_us(
        || {
            sink(store.worktree.clone());
        },
        5,
    );

    let all_files = median_us(|| sink(materialize_files(&store, 0..2_000)), 3);
    let window_files = median_us(
        || {
            let range = visible_window(2_000, 800.0, 600.0, 28.0, OVERSCAN);
            sink(materialize_files(&store, range));
        },
        5,
    );

    let diff = seed_diff(5_000);
    let diff_build = time_us(|| sink(seed_diff(5_000)));
    let diff_all = median_us(|| sink(header_scan(&diff, diff.len())), 5);
    let diff_window = median_us(
        || {
            let range = visible_window(diff.len(), 2_000.0, 600.0, 26.0, OVERSCAN);
            sink(header_scan(&diff, range.end) - header_scan(&diff, range.start));
        },
        5,
    );

    let snapshots = seed_snapshots(2_000);
    let history_all = median_us(
        || {
            let cloned = snapshots.clone();
            sink(cloned.iter().map(|shot| shot.title.len()).sum::<usize>());
        },
        3,
    );
    let history_window = median_us(
        || {
            let range = visible_window(snapshots.len(), 400.0, 600.0, 36.0, OVERSCAN);
            sink(
                snapshots[range]
                    .iter()
                    .map(|shot| shot.title.len())
                    .sum::<usize>(),
            );
        },
        5,
    );

    let branches = seed_branches(2_000);
    let filter_all = median_us(
        || {
            sink(super::pages::visible_indices(
                &branches,
                super::pages::BranchTab::Local,
                "",
            ))
        },
        5,
    );
    store.set_branch_rows(branches);
    store.pages.tab = super::pages::BranchTab::Local;
    let _ = store.branch_indexes();
    let filter_cached = median_us(|| sink(store.branch_indexes()), 5);

    let naive = 200u32;
    let coalesced = coalesced_refreshes(200, 50);

    let mut fractions = super::COLUMN_DEFAULT;
    let drag = median_us(
        || {
            for step in 0..40 {
                fractions = super::drag_handle(fractions, step % 2, 4.0, 1280.0);
            }
        },
        5,
    );

    serde_json::json!({
        "idle": {
            "cached_group_us": cached_group,
            "note": "No list work. The cached selector is the idle cost of asking what to draw."
        },
        "scroll-long-diff": {
            "lines": diff.len(),
            "build_rows_us": diff_build,
            "before_header_scan_us": diff_all,
            "after_visible_header_us": diff_window,
            "item_px": 26
        },
        "long-file-list": {
            "files": 2000,
            "before_group_us": uncached_group,
            "after_group_us": cached_group,
            "before_materialize_us": all_files,
            "after_materialize_us": window_files,
            "before_deep_clone_us": deep_clone,
            "after_arc_clone_us": arc_clone
        },
        "long-history": {
            "commits": 2000,
            "before_lane_rows_us": uncached_lanes,
            "after_lane_rows_us": cached_lanes,
            "snapshots": 2000,
            "before_snapshot_clone_us": history_all,
            "after_snapshot_window_us": history_window,
            "before_branch_filter_us": filter_all,
            "after_branch_filter_us": filter_cached
        },
        "watcher-burst": {
            "events": 200,
            "before_refreshes": naive,
            "after_refreshes": coalesced,
            "delay_ms": 50
        },
        "column-drag": {
            "steps": 40,
            "median_us": drag
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::FileSlot;
    use super::*;

    #[test]
    fn visible_window_clamps_and_overscans() {
        assert_eq!(visible_window(0, 0.0, 600.0, 28.0, 4), 0..0);
        assert_eq!(visible_window(100, 0.0, 280.0, 28.0, 0), 0..10);
        let mid = visible_window(100, 280.0, 280.0, 28.0, 2);
        assert_eq!(mid.start, 8);
        assert_eq!(mid.end, 22);
        assert!(mid.end <= 100);
        let past = visible_window(10, 10_000.0, 280.0, 28.0, 4);
        assert_eq!(past.end, 10);
        assert!(past.start <= 10);
        assert!(past.start <= past.end);
    }

    #[test]
    fn visible_window_tiles_cover_exactly_the_full_list_when_scrolled_through() {
        let len = 3_000usize;
        let item_px = 28.0f32;
        let viewport_px = 720.0f32;
        let overscan = 2usize;
        let mut covered = std::collections::BTreeSet::new();
        let mut scroll = 0.0f32;
        let max_scroll = len as f32 * item_px;
        while scroll <= max_scroll {
            for index in visible_window(len, scroll, viewport_px, item_px, overscan) {
                covered.insert(index);
            }
            scroll += item_px;
        }
        // The union of painted windows equals what the old full list rendered:
        // every row at least once, nothing outside 0..len.
        let expected: std::collections::BTreeSet<usize> = (0..len).collect();
        assert_eq!(covered, expected);
    }

    #[test]
    fn two_hundred_events_start_at_most_two_refreshes() {
        let started = coalesced_refreshes(200, 50);
        assert!(started >= 1, "a burst still refreshes once");
        assert!(started <= 2, "got {started} refreshes");
    }

    #[test]
    fn a_quiet_batch_is_one_refresh_and_a_trailing_event_is_the_second() {
        let mut gate = RefreshGate::new(50);
        for now in 0..10 {
            gate.note(now);
        }
        assert!(!gate.poll(10));
        assert!(gate.poll(50));
        assert!(gate.in_flight());
        assert_eq!(gate.started(), 1);
        gate.note(60);
        assert!(!gate.poll(80));
        gate.finish(90);
        assert!(gate.poll(90));
        assert_eq!(gate.started(), 2);
        gate.finish(100);
        assert!(!gate.poll(100));
        assert_eq!(gate.started(), 2);
    }

    #[test]
    fn selectors_reuse_groups_lanes_and_branch_indexes() {
        let mut store = Store {
            worktree: Some(Arc::new(seed_files(80))),
            workspace: Some(Arc::new(seed_workspace(40))),
            ..Store::default()
        };
        let groups = store.change_groups();
        assert!(!groups.is_empty());
        assert_eq!(store.group_rebuilds(), 1);
        assert_eq!(store.change_groups(), groups);
        assert_eq!(store.group_rebuilds(), 1);
        let flat = store.flat_files();
        assert_eq!(flat.len(), 80);
        assert_eq!(store.group_rebuilds(), 1);

        let lanes = store.lane_rows();
        assert_eq!(lanes.len(), 41);
        assert_eq!(store.lane_rebuilds(), 1);
        assert_eq!(store.lane_rows().len(), 41);
        assert_eq!(store.lane_rebuilds(), 1);

        store.worktree = Some(Arc::new(seed_files(81)));
        let _ = store.change_groups();
        assert_eq!(store.group_rebuilds(), 2);

        store.pages.branch_rows = seed_branches(30);
        store.pages.tab = super::super::pages::BranchTab::Local;
        let indexes = store.branch_indexes();
        assert_eq!(indexes.len(), 30);
        assert_eq!(store.branch_filter_rebuilds(), 1);
        store.set_branch_filter("topic-0001".to_string());
        let filtered = store.branch_indexes();
        assert_eq!(filtered.len(), 1);
        assert_eq!(store.branch_filter_rebuilds(), 2);
        assert_eq!(store.branch_indexes(), filtered);
        assert_eq!(store.branch_filter_rebuilds(), 2);
    }

    #[test]
    fn show_more_raises_the_page_without_a_file() {
        let mut store = Store::default();
        assert_eq!(store.diff_limit, DIFF_PAGE);
        assert!(matches!(store.show_more_diff(), super::super::Effect::None));
        assert_eq!(
            store.diff_limit,
            DIFF_PAGE + gb_bridge::DIFF_ROW_CAP.max(DIFF_PAGE)
        );
    }

    #[test]
    fn measure_covers_every_scenario() {
        let value = measure();
        for name in [
            "idle",
            "scroll-long-diff",
            "long-file-list",
            "long-history",
            "watcher-burst",
            "column-drag",
        ] {
            assert!(value.get(name).is_some(), "{name}");
        }
        let refreshes = value["watcher-burst"]["after_refreshes"].as_u64().unwrap();
        assert!(refreshes <= 2, "{refreshes}");
        assert!(value["scroll-long-diff"]["lines"].as_u64().unwrap() >= 5_000);
    }

    #[test]
    fn file_slots_match_the_grouped_list() {
        let store = Store {
            worktree: Some(Arc::new(seed_files(25))),
            workspace: Some(Arc::new(seed_workspace(1))),
            ..Store::default()
        };
        let slots = store.file_slots();
        let files = slots
            .iter()
            .filter(|slot| matches!(slot, FileSlot::File { .. }))
            .count();
        let headers = slots.len() - files;
        assert_eq!(files, 25);
        assert!(headers >= 1);
        assert!(slots.len() < 25 * 2);
    }
}
