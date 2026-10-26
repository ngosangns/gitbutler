//! Frame clock and the dev-socket `perf` report. Fake data only.
//!
//! `render()` times element build with [`RenderClock`]. The next draw settles
//! the sample. A covered window stops the display link, so the run schedules
//! the next draw itself. `render_us` is that element-build time.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use gpui::{SharedString, UniformListScrollHandle, point, px};

use crate::store::{DiffRow, RowKind};

/// Rows in one lane list before the column switches off the card layout.
pub const LANE_CARD_MAX: usize = 48;

/// Fixed height for every virtual lane row. `uniform_list` measures item 0 and
/// spaces every row by that height; headers are short and commit rows are two
/// lines, so a content-sized first row collapses later commits on top of each
/// other.
pub const LANE_ROW_HEIGHT: f32 = 56.0;

const SCENARIOS: &[&str] = &[
    "idle",
    "scroll-long-diff",
    "long-file-list",
    "long-history",
    "watcher-burst",
    "column-drag",
    "large-tree",
    "large-list-marked",
    "branch-detail",
    "branch-search",
    "pr-tab",
    "snapshot-files",
];

/// One perf pass. The window holds this in an `Rc<RefCell<_>>` so `render`
/// can record a time without borrowing the view.
#[derive(Debug)]
pub struct PerfRun {
    pub recording: bool,
    pub frame_armed: bool,
    pub label: String,
    pub full: bool,
    pub names: Vec<String>,
    pub index: usize,
    pub current: String,
    pub frames_target: u32,
    pub frames_done: u32,
    pub started: Instant,
    pub renders: Vec<u64>,
    pub paint_base: u64,
    pub notify_base: u64,
    pub gate_base: u32,
    pub samples: Vec<serde_json::Value>,
    pub wrote: bool,
    pub path: String,
    pub error: Option<String>,
}

impl Default for PerfRun {
    fn default() -> Self {
        Self {
            recording: false,
            frame_armed: false,
            label: String::new(),
            full: false,
            names: Vec::new(),
            index: 0,
            current: String::new(),
            frames_target: 0,
            frames_done: 0,
            started: Instant::now(),
            renders: Vec::new(),
            paint_base: 0,
            notify_base: 0,
            gate_base: 0,
            samples: Vec::new(),
            wrote: false,
            path: String::new(),
            error: None,
        }
    }
}

/// Drop at the end of `render` to store the element-build time.
pub struct RenderClock {
    pub start: Instant,
    pub sink: Rc<RefCell<PerfRun>>,
}

impl Drop for RenderClock {
    fn drop(&mut self) {
        let micros = self.start.elapsed().as_micros();
        let micros = u64::try_from(micros).unwrap_or(u64::MAX);
        let mut run = self.sink.borrow_mut();
        if run.recording {
            run.renders.push(micros);
        }
    }
}

pub fn perf_names(scenario: &str) -> Result<Vec<String>, String> {
    if scenario.is_empty() || scenario == "all" {
        return Ok(SCENARIOS.iter().map(|name| (*name).to_string()).collect());
    }
    if SCENARIOS.contains(&scenario) {
        return Ok(vec![scenario.to_string()]);
    }
    Err(format!(
        "perf scenario is all, {}, got {scenario}",
        SCENARIOS.join(", ")
    ))
}

pub fn output_path(label: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".agent-scratch/gb-gpui")
        .join(format!("perf-{label}.json"))
}

pub fn profile_name() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    }
}

pub fn machine_info() -> serde_json::Value {
    serde_json::json!({
        "model": sysctl_n("hw.model"),
        "cpu": sysctl_n("machdep.cpu.brand_string"),
        "os": sysctl_n("kern.osproductversion"),
        "profile": profile_name(),
    })
}

fn sysctl_n(key: &str) -> String {
    std::process::Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn scenario_sample(
    name: &str,
    frames: u32,
    elapsed_ms: u64,
    renders: &[u64],
    painted: u64,
    notifies: u64,
    refreshes: u32,
) -> serde_json::Value {
    let fps = if elapsed_ms == 0 {
        0.0
    } else {
        f64::from(frames) * 1000.0 / elapsed_ms as f64
    };
    let notifies_per_s = if elapsed_ms == 0 {
        0.0
    } else {
        notifies as f64 * 1000.0 / elapsed_ms as f64
    };
    serde_json::json!({
        "name": name,
        "frames": frames,
        "elapsed_ms": elapsed_ms,
        "fps": (fps * 10.0).round() / 10.0,
        "render_us_first": renders.first().copied().unwrap_or(0),
        "render_us_median": median(renders),
        "painted_rows": painted,
        "notifies": notifies,
        "notifies_per_s": (notifies_per_s * 10.0).round() / 10.0,
        "refreshes": refreshes,
    })
}

fn median(samples: &[u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

pub fn write_report(path: &std::path::Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| error.to_string())
}

/// Positive pixels the uniform list has scrolled. The handle stores a negated y.
pub fn list_offset(handle: &UniformListScrollHandle) -> f32 {
    -handle.0.borrow().base_handle.offset().y.as_f32()
}

pub fn set_list_offset(handle: &UniformListScrollHandle, y: i32) {
    let y = -(y as f32);
    handle
        .0
        .borrow()
        .base_handle
        .set_offset(point(px(0.), px(y)));
}

/// Reuse a `SharedString` when the same line is drawn again.
pub fn intern_line(cache: &Rc<RefCell<HashMap<u64, SharedString>>>, text: &str) -> SharedString {
    let hash = fnv(text);
    {
        let map = cache.borrow();
        if let Some(hit) = map.get(&hash)
            && hit.as_ref() == text
        {
            return hit.clone();
        }
    }
    let mut map = cache.borrow_mut();
    if map.len() > 8_000 {
        map.clear();
    }
    let shared = SharedString::from(text.to_string());
    map.insert(hash, shared.clone());
    shared
}

fn fnv(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn diff_truncated(rows: &[DiffRow]) -> bool {
    rows.last()
        .is_some_and(|row| row.kind == RowKind::Note && row.text.starts_with("Diff too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_reuses_one_entry_for_the_same_line() {
        let cache = Rc::new(RefCell::new(HashMap::new()));
        let first = intern_line(&cache, "same line");
        let second = intern_line(&cache, "same line");
        assert_eq!(first, second);
        assert_eq!(cache.borrow().len(), 1);
        let _ = intern_line(&cache, "other line");
        assert_eq!(cache.borrow().len(), 2);
    }

    #[test]
    fn a_capped_diff_note_asks_for_more() {
        let rows = vec![DiffRow {
            kind: RowKind::Note,
            old_line: None,
            new_line: None,
            text: "Diff too large — showing first 400 lines".to_string(),
            hunk: None,
            lane: None,
        }];
        assert!(diff_truncated(&rows));
        assert!(!diff_truncated(&[]));
    }

    #[test]
    fn scenario_names_reject_unknown_input() {
        assert_eq!(perf_names("all").unwrap().len(), SCENARIOS.len());
        assert!(perf_names("nope").is_err());
        assert_eq!(perf_names("idle").unwrap(), vec!["idle".to_string()]);
    }
}
