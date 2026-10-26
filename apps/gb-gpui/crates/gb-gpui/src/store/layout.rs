//! Column widths for the workspace page.
//!
//! Fractions are what gets stored, so a later window size keeps the same
//! split. Pixel widths are derived and clamped. The file is gb-gpui's own
//! UI state. It is never GitButler's app data.

use std::fs;
use std::path::{Path, PathBuf};

/// Narrowest comfortable width for Uncommitted, Changes, and Branches.
pub const COLUMN_MIN: [f32; 3] = [200.0, 280.0, 220.0];

/// Share of the space between the two drag handles.
pub const COLUMN_DEFAULT: [f32; 3] = [0.28, 0.40, 0.32];

/// Width of one drag handle, in points.
pub const HANDLE_WIDTH: f32 = 8.0;

const VERSION: u64 = 1;

/// Points left for the three columns after the handles are subtracted.
pub fn content_width(viewport: f32) -> f32 {
    (viewport - 2.0 * HANDLE_WIDTH).max(0.0)
}

/// Pixel widths that sum to [`content_width`]. A short window scales the
/// minimums down so the row still fits.
pub fn column_pixels(fractions: [f32; 3], viewport: f32) -> [f32; 3] {
    let available = content_width(viewport);
    if available <= 0.0 {
        return [0.0; 3];
    }
    let sum_min: f32 = COLUMN_MIN.iter().sum();
    if available + 0.5 < sum_min {
        let scale = available / sum_min;
        let mut widths = [
            COLUMN_MIN[0] * scale,
            COLUMN_MIN[1] * scale,
            COLUMN_MIN[2] * scale,
        ];
        settle(&mut widths, available);
        return widths;
    }
    let fractions = sane_fractions(fractions);
    let mut widths = [
        fractions[0] * available,
        fractions[1] * available,
        fractions[2] * available,
    ];
    for _ in 0..3 {
        let mut deficit = 0.0;
        let mut slack = 0.0;
        for index in 0..3 {
            if widths[index] < COLUMN_MIN[index] {
                deficit += COLUMN_MIN[index] - widths[index];
                widths[index] = COLUMN_MIN[index];
            } else {
                slack += widths[index] - COLUMN_MIN[index];
            }
        }
        if deficit <= 0.5 || slack <= 0.0 {
            break;
        }
        let take = deficit.min(slack);
        for index in 0..3 {
            let room = widths[index] - COLUMN_MIN[index];
            if room > 0.0 {
                widths[index] -= take * (room / slack);
            }
        }
    }
    settle(&mut widths, available);
    widths
}

/// Move the divider `handle` (0 between the first pair, 1 between the second)
/// by `delta` points. The other column keeps its width.
pub fn drag_handle(fractions: [f32; 3], handle: usize, delta: f32, viewport: f32) -> [f32; 3] {
    if handle > 1 || !delta.is_finite() {
        return sane_fractions(fractions);
    }
    let mut widths = column_pixels(fractions, viewport);
    let left = handle;
    let right = handle + 1;
    let pair = widths[left] + widths[right];
    let available = content_width(viewport);
    let sum_min: f32 = COLUMN_MIN.iter().sum();
    let (min_left, min_right) = if available + 0.5 < sum_min {
        (0.0, 0.0)
    } else {
        (COLUMN_MIN[left], COLUMN_MIN[right])
    };
    if pair <= min_left + min_right + 0.5 {
        return fractions_from_pixels(widths);
    }
    let next = (widths[left] + delta).clamp(min_left, pair - min_right);
    widths[left] = next;
    widths[right] = pair - next;
    fractions_from_pixels(widths)
}

pub fn fractions_from_pixels(pixels: [f32; 3]) -> [f32; 3] {
    let sum: f32 = pixels.iter().sum();
    if sum <= 0.0 {
        return COLUMN_DEFAULT;
    }
    sane_fractions([pixels[0] / sum, pixels[1] / sum, pixels[2] / sum])
}

pub fn sane_fractions(values: [f32; 3]) -> [f32; 3] {
    let sum: f32 = values.iter().filter(|value| value.is_finite()).sum();
    if sum <= 0.0
        || values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return COLUMN_DEFAULT;
    }
    [values[0] / sum, values[1] / sum, values[2] / sum]
}

/// Keep a popup of `width` by `height` inside the window, `margin` points in.
pub fn clamp_popup(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    view_w: f32,
    view_h: f32,
    margin: f32,
) -> (f32, f32) {
    let margin = margin.max(0.0);
    let max_x = (view_w - width - margin).max(margin);
    let max_y = (view_h - height - margin).max(margin);
    let x = if x.is_finite() { x } else { margin };
    let y = if y.is_finite() { y } else { margin };
    (x.clamp(margin, max_x), y.clamp(margin, max_y))
}

pub fn ui_state_file(dir: &Path) -> PathBuf {
    dir.join("ui-state.json")
}

/// Where a real window reads column widths. Fake mode stays in memory unless
/// `GB_GPUI_UI_DIR` points somewhere that is not GitButler's data.
pub fn persistent_file() -> Option<PathBuf> {
    let fake = std::env::var_os("GB_GPUI_FAKE").is_some();
    let dir = match std::env::var_os("GB_GPUI_UI_DIR") {
        Some(dir) => PathBuf::from(dir),
        None if fake => return None,
        None => default_ui_dir(),
    };
    let file = ui_state_file(&dir);
    if protected_path(&file) {
        None
    } else {
        Some(file)
    }
}

pub fn default_ui_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/gb-gpui")
    } else {
        home.join(".config/gb-gpui")
    }
}

/// True when `path` would land in GitButler's app data, config, or a repo store.
pub fn protected_path(path: &Path) -> bool {
    let text = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if text.contains("com.gitbutler.app")
        || text.contains("/.git/gitbutler")
        || text.contains("application support/gitbutler")
        || text.contains("/gitbutler/ui-state")
    {
        return true;
    }
    let data = gb_bridge::compiled_app_data_dir();
    !data.as_os_str().is_empty() && path.starts_with(&data)
}

pub fn load_fractions(path: &Path) -> [f32; 3] {
    if protected_path(path) {
        return COLUMN_DEFAULT;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return COLUMN_DEFAULT;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return COLUMN_DEFAULT;
    };
    let Some(columns) = value.get("columns").and_then(|item| item.as_array()) else {
        return COLUMN_DEFAULT;
    };
    if columns.len() != 3 {
        return COLUMN_DEFAULT;
    }
    let mut fractions = [0.0; 3];
    for (index, item) in columns.iter().enumerate() {
        let Some(number) = item.as_f64() else {
            return COLUMN_DEFAULT;
        };
        fractions[index] = number as f32;
    }
    sane_fractions(fractions)
}

pub fn save_fractions(path: &Path, fractions: [f32; 3]) -> Result<(), String> {
    if protected_path(path) {
        return Err("Refusing to write UI state inside GitButler's data.".to_string());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let fractions = sane_fractions(fractions);
    let body = serde_json::json!({
        "version": VERSION,
        "columns": fractions,
    });
    fs::write(path, format!("{body}\n")).map_err(|error| error.to_string())
}

fn settle(widths: &mut [f32; 3], available: f32) {
    let sum: f32 = widths.iter().sum();
    widths[2] = (widths[2] + (available - sum)).max(0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_window_scales_without_going_negative() {
        let widths = column_pixels(COLUMN_DEFAULT, 400.0);
        assert!(
            widths
                .iter()
                .all(|width| width.is_finite() && *width >= 0.0)
        );
        let sum: f32 = widths.iter().sum();
        assert!(
            (sum - content_width(400.0)).abs() < 1.0,
            "{widths:?} sum {sum}"
        );
        assert!(widths.iter().all(|width| *width < 200.0));
    }

    #[test]
    fn drag_respects_the_minimum_and_zero_delta_stays_put() {
        let start = COLUMN_DEFAULT;
        let same = drag_handle(start, 0, 0.0, 1280.0);
        let before = column_pixels(start, 1280.0);
        let after = column_pixels(same, 1280.0);
        for index in 0..3 {
            assert!((before[index] - after[index]).abs() < 1.0);
        }
        let pushed = drag_handle(start, 0, 5_000.0, 1280.0);
        let widths = column_pixels(pushed, 1280.0);
        // The divider only trades width with its neighbor. The middle hits its
        // minimum and the branches column stays where it was.
        let expected_left = before[0] + before[1] - COLUMN_MIN[1];
        assert!(
            (widths[0] - expected_left).abs() < 1.0,
            "{widths:?} expected left {expected_left}"
        );
        assert!((widths[1] - COLUMN_MIN[1]).abs() < 1.0, "{widths:?}");
        assert!((widths[2] - before[2]).abs() < 1.0, "{widths:?}");
        let pulled = drag_handle(start, 1, -5_000.0, 1280.0);
        let widths = column_pixels(pulled, 1280.0);
        assert!((widths[0] - before[0]).abs() < 1.0, "{widths:?}");
        assert!((widths[1] - COLUMN_MIN[1]).abs() < 1.0, "{widths:?}");
        assert!(widths[2] + 0.5 >= COLUMN_MIN[2]);
    }

    #[test]
    fn reset_is_the_default_split() {
        let dragged = drag_handle(COLUMN_DEFAULT, 0, 160.0, 1280.0);
        assert!((dragged[0] - COLUMN_DEFAULT[0]).abs() > 0.02);
        let reset = COLUMN_DEFAULT;
        let widths = column_pixels(reset, 1280.0);
        assert!(widths[0] < widths[1]);
        assert!((widths.iter().sum::<f32>() - content_width(1280.0)).abs() < 1.0);
    }

    #[test]
    fn fractions_round_trip_through_a_temp_file() {
        let dir =
            std::env::temp_dir().join(format!("gb-gpui-ui-{}-{}", std::process::id(), "s9-layout"));
        let _ = fs::remove_dir_all(&dir);
        let path = ui_state_file(&dir);
        let dragged = drag_handle(COLUMN_DEFAULT, 0, 140.0, 1280.0);
        save_fractions(&path, dragged).unwrap();
        let loaded = load_fractions(&path);
        for index in 0..3 {
            assert!((loaded[index] - dragged[index]).abs() < 0.001, "{loaded:?}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gitbutler_paths_are_refused() {
        let path = PathBuf::from("/tmp/com.gitbutler.app/ui-state.json");
        assert!(protected_path(&path));
        assert!(save_fractions(&path, COLUMN_DEFAULT).is_err());
        assert!(!path.exists());
        assert!(protected_path(Path::new(
            "/tmp/repo/.git/gitbutler/ui-state.json"
        )));
        let own = default_ui_dir().join("ui-state.json");
        assert!(!protected_path(&own), "{}", own.display());
    }

    #[test]
    fn a_popup_stays_inside_the_window() {
        let (x, y) = clamp_popup(1_200.0, 800.0, 280.0, 200.0, 1280.0, 860.0, 8.0);
        assert!(x + 280.0 <= 1280.0 - 8.0 + 0.5);
        assert!(y + 200.0 <= 860.0 - 8.0 + 0.5);
        let (x, y) = clamp_popup(-40.0, -10.0, 280.0, 40.0, 1280.0, 860.0, 8.0);
        assert!((x - 8.0).abs() < 0.1);
        assert!((y - 8.0).abs() < 0.1);
    }
}
